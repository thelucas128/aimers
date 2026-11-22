//! RUSTUDIO_GPU_TIMING=1: GPU time per pass group, from timestamp queries.
//!
//! `begin(device, encoder)` writes a timestamp; each `mark(encoder, label)`
//! writes another and books the GPU time since the previous one to `label`.
//! `end_frame` resolves the frame's queries into a read-back buffer (a ring
//! of a few, so nothing waits for the GPU) and collects frames that have
//! finished. `take()` hands the bench the per-label milliseconds and each
//! finished frame's total (the sum of its spans: the timed encoders only, not
//! egui's own pass or the present). Results lag the frame they measure by a
//! frame or two. Needs the adapter's TIMESTAMP_QUERY and
//! TIMESTAMP_QUERY_INSIDE_ENCODERS (requested only with the switch on);
//! without them, or with the switch off, every call is a no-op.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use egui_wgpu::wgpu;

const MAX_QUERIES: u32 = 256;
const RING: usize = 4;
const FREE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;

pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RUSTUDIO_GPU_TIMING").is_some())
}

/// The device features the switch needs, from what the adapter offers.
pub fn features(adapter: wgpu::Features) -> wgpu::Features {
    if on() {
        adapter & (wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
    } else {
        wgpu::Features::empty()
    }
}

struct Slot {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    read: wgpu::Buffer,
    spans: Vec<(u32, u32, &'static str)>,
    used: u32,
    state: Arc<AtomicU8>,
}

struct State {
    slots: Vec<Slot>,
    cur: Option<usize>,
    last: Option<u32>,
    period_ns: f64,
    results: Vec<(&'static str, f64)>,
    totals: Vec<f64>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static UNSUPPORTED: OnceLock<()> = OnceLock::new();

fn init(device: &wgpu::Device, queue: &wgpu::Queue) -> Option<State> {
    let need = wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
    if !device.features().contains(need) {
        UNSUPPORTED.get_or_init(|| eprintln!("RUSTUDIO_GPU_TIMING: the adapter has no timestamp queries inside encoders"));
        return None;
    }
    // The bench (core) asks for the timings through this callback.
    rustudio_core::bench::set_gpu_timing(take);
    let bytes = MAX_QUERIES as u64 * 8;
    let slots = (0..RING)
        .map(|_| Slot {
            set: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("gpu timing"),
                ty: wgpu::QueryType::Timestamp,
                count: MAX_QUERIES,
            }),
            resolve: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("gpu timing resolve"),
                size: bytes,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            read: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("gpu timing read"),
                size: bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            spans: Vec::new(),
            used: 0,
            state: Arc::new(AtomicU8::new(FREE)),
        })
        .collect();
    Some(State {
        slots,
        cur: Some(0),
        last: None,
        period_ns: queue.get_timestamp_period() as f64,
        results: Vec::new(),
        totals: Vec::new(),
    })
}


fn write(st: &mut State, encoder: &mut wgpu::CommandEncoder) -> Option<u32> {
    let slot = &mut st.slots[st.cur?];
    if slot.used >= MAX_QUERIES {
        return None;
    }
    let i = slot.used;
    encoder.write_timestamp(&slot.set, i);
    slot.used += 1;
    Some(i)
}

/// Starts a timed run of work in `encoder` (each encoder that is timed).
pub fn begin(device: &wgpu::Device, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder) {
    if !on() || UNSUPPORTED.get().is_some() {
        return;
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = init(device, queue);
    }
    let Some(st) = g.as_mut() else { return };
    st.last = write(st, encoder);
}

/// Books the GPU time since the last `begin` / `mark` in this encoder to `label`.
pub fn mark(encoder: &mut wgpu::CommandEncoder, label: &'static str) {
    if !on() {
        return;
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = g.as_mut() else { return };
    let Some(prev) = st.last else { return };
    let Some(i) = write(st, encoder) else { return };
    let cur = st.cur.unwrap_or(0);
    st.slots[cur].spans.push((prev, i, label));
    st.last = Some(i);
}

/// Once per frame, after the frame's work is submitted: resolve this frame's
/// queries and collect earlier frames that finished.
pub fn end_frame(device: &wgpu::Device, queue: &wgpu::Queue) {
    if !on() {
        return;
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(st) = g.as_mut() else { return };
    if let Some(cur) = st.cur {
        let slot = &mut st.slots[cur];
        if slot.used > 0 && !slot.spans.is_empty() {
            let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gpu timing") });
            enc.resolve_query_set(&slot.set, 0..slot.used, &slot.resolve, 0);
            enc.copy_buffer_to_buffer(&slot.resolve, 0, &slot.read, 0, slot.used as u64 * 8);
            queue.submit([enc.finish()]);
            slot.state.store(MAPPING, Ordering::Release);
            let state = slot.state.clone();
            slot.read.slice(..slot.used as u64 * 8).map_async(wgpu::MapMode::Read, move |r| {
                state.store(if r.is_ok() { MAPPED } else { FREE }, Ordering::Release);
            });
        } else {
            slot.used = 0;
            slot.spans.clear();
        }
    }
    st.last = None;
    let _ = device.poll(wgpu::PollType::Poll);
    let period = st.period_ns;
    for slot in st.slots.iter_mut() {
        if slot.state.load(Ordering::Acquire) != MAPPED {
            continue;
        }
        if let Ok(data) = slot.read.slice(..slot.used as u64 * 8).get_mapped_range() {
            let ts: Vec<u64> = data.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0; 8]))).collect();
            let mut total = 0.0;
            for (a, b, label) in &slot.spans {
                let ms = ts[*b as usize].wrapping_sub(ts[*a as usize]) as f64 * period / 1e6;
                // (a counter that went backwards or a bogus value: dropped)
                if !(0.0..1000.0).contains(&ms) {
                    continue;
                }
                total += ms;
                match st.results.iter_mut().find(|e| e.0 == *label) {
                    Some(e) => e.1 += ms,
                    None => st.results.push((label, ms)),
                }
            }
            st.totals.push(total);
        }
        slot.read.unmap();
        slot.used = 0;
        slot.spans.clear();
        slot.state.store(FREE, Ordering::Release);
    }
    // The next free slot records the next frame; none free: that frame isn't timed.
    st.cur = st.slots.iter().position(|s| s.state.load(Ordering::Acquire) == FREE && s.used == 0);
}

/// The GPU milliseconds per label since the last call, and each finished
/// frame's total.
pub fn take() -> (Vec<(&'static str, f64)>, Vec<f64>) {
    if !on() {
        return (Vec::new(), Vec::new());
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    match g.as_mut() {
        Some(st) => (std::mem::take(&mut st.results), std::mem::take(&mut st.totals)),
        None => (Vec::new(), Vec::new()),
    }
}

/// `queue.submit(bufs)`, timed as `label` (a timestamp before and after,
/// in their own command buffers) with the switch on. Doesn't disturb a
/// `begin`/`mark` run being recorded in another encoder meanwhile.
pub fn submit(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &'static str,
    bufs: impl IntoIterator<Item = wgpu::CommandBuffer>,
) {
    if !on() || UNSUPPORTED.get().is_some() {
        queue.submit(bufs);
        return;
    }
    let saved = STATE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|s| s.last);
    let mut a = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gpu timing") });
    begin(device, queue, &mut a);
    let mut b = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gpu timing") });
    mark(&mut b, label);
    if let Some(st) = STATE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        st.last = saved;
    }
    queue.submit(std::iter::once(a.finish()).chain(bufs).chain(std::iter::once(b.finish())));
}
