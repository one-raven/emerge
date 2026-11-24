//! One Raven patch: SVG rasterization off the render thread.
//!
//! A vector variant (an SVG at one pixel size) used to be rasterized by resvg
//! inside the first frame that drew it. A screen of generated art, a dozen
//! 357 x 330 room pictures say, cost ~28 ms of that frame on an M-series Mac
//! and ~250 ms on a Cortex-A55. As a browser decodes images off its main
//! thread, a renderer that opts in (`SceneRenderer::set_async_vector_rasterization`)
//! now rasterizes a miss in the frame only while the frame's predicted
//! rasterization time stays within `FRAME_BUDGET`, so icons are still drawn
//! at once. The prediction is pixels times the asset's own ns/pixel from an
//! earlier size, or `UNKNOWN_NS_PER_PIXEL` (cost per pixel differs ~50x
//! between a flat chart and gradient-heavy art, so one global rate misleads). A larger miss goes to a
//! small worker pool and the frame draws the nearest cached size of the same
//! picture, scaled, or nothing. When the pool's queue for that asset context
//! drains (or every `NOTIFY_INTERVAL` while it doesn't), the asset worker asks
//! the tree for a new frame. Storing the variant bumped the record's render
//! revision, so that frame's paint-layer and base-snapshot keys miss and the
//! picture is drawn.
//!
//! Only with a running asset worker (`assets::can_request_tree_update`): a bare
//! renderer has nobody to ask for the next frame and stays synchronous.

use super::*;
use crossbeam_channel::{Sender, unbounded};
use std::cell::Cell;
use std::sync::OnceLock;

/// Rasterization a frame may still do in place before misses are deferred.
const FRAME_BUDGET: Duration = Duration::from_millis(4);
/// An asset not rasterized before. Room art measures ~20 ns/px on an M-series
/// Mac and ~170 on an A55; this defers anything over ~80k px (280 x 280).
const UNKNOWN_NS_PER_PIXEL: f64 = 50.0;
/// Smaller rasterizations are dominated by fixed costs; don't learn from them.
const MIN_SAMPLE_PIXELS: u64 = 64 * 64;
/// Learned rates kept; the map is cleared when it grows past this.
const MAX_LEARNED_RATES: usize = 4096;
/// While misses keep arriving, ask for a frame at least this often.
const NOTIFY_INTERVAL: Duration = Duration::from_millis(50);
/// A cached size stands in for a pending one only with the same shape.
const STAND_IN_ASPECT_TOLERANCE: f32 = 0.02;

/// Measured ns/pixel by asset id (ids are content hashes).
static RATES: OnceLock<Mutex<HashMap<String, f64>>> = OnceLock::new();

fn rates() -> &'static Mutex<HashMap<String, f64>> {
    RATES.get_or_init(Default::default)
}

thread_local! {
    /// Remaining in-frame budget (ns) while an opted-in renderer draws.
    static FRAME_BUDGET_NS: Cell<Option<f64>> = const { Cell::new(None) };
}

pub(super) struct FrameGuard {
    previous: Option<f64>,
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        FRAME_BUDGET_NS.with(|budget| budget.set(self.previous));
    }
}

/// Start a frame's budget (or none, when the renderer hasn't opted in).
pub(super) fn enter_frame(enabled: bool) -> FrameGuard {
    let budget = enabled.then(|| FRAME_BUDGET.as_nanos() as f64);
    FrameGuard {
        previous: FRAME_BUDGET_NS.with(|slot| slot.replace(budget)),
    }
}

pub(super) fn record_rasterization(asset_id: &str, width: u32, height: u32, elapsed: Duration) {
    let pixels = u64::from(width) * u64::from(height);
    if pixels < MIN_SAMPLE_PIXELS {
        return;
    }
    let sample = elapsed.as_nanos() as f64 / pixels as f64;
    let Ok(mut rates) = rates().lock() else {
        return;
    };
    if rates.len() >= MAX_LEARNED_RATES && !rates.contains_key(asset_id) {
        rates.clear();
    }
    rates
        .entry(asset_id.to_string())
        .and_modify(|rate| *rate = *rate * 0.5 + sample * 0.5)
        .or_insert(sample);
}

fn ns_per_pixel(asset_id: &str) -> f64 {
    #[cfg(test)]
    if let Some(rate) = TEST_NS_PER_PIXEL.with(Cell::get) {
        return rate;
    }
    rates()
        .lock()
        .ok()
        .and_then(|rates| rates.get(asset_id).copied())
        .unwrap_or(UNKNOWN_NS_PER_PIXEL)
}

/// `None`: rasterize this miss now. `Some(stand_in)`: it is (now) being
/// rasterized on the pool; draw `stand_in`, a cached size of the same
/// picture, if there is one.
pub(super) fn defer(
    asset_id: &str,
    width: u32,
    height: u32,
    kind: RenderedVectorVariantKind,
) -> Option<Option<Image>> {
    let remaining = FRAME_BUDGET_NS.with(Cell::get)?;
    let predicted = f64::from(width) * f64::from(height) * ns_per_pixel(asset_id);
    if predicted <= remaining {
        FRAME_BUDGET_NS.with(|budget| budget.set(Some(remaining - predicted)));
        return None;
    }
    if !crate::assets::can_request_tree_update() {
        return None;
    }
    let record = scene_images::record(asset_id)?;
    let crate::assets::AssetRecordKind::Vector(tree) = &record.kind else {
        return None;
    };
    if !record_is_current_for_cache(&record) {
        return None;
    }
    let tree = Arc::clone(tree);
    let key = rendered_vector_key(asset_id, width, height, kind);
    let context = asset_context();
    let mut cache = context.pixel_cache.lock().ok()?;
    if !cache.pending_vectors.contains(&key) {
        if cache.pending_vectors.is_empty() {
            cache.pending_notified_at = Some(Instant::now());
        }
        let job = Job {
            context: crate::assets::current_context(),
            record: Arc::clone(&record),
            tree,
            key: key.clone(),
        };
        pool().send(job).ok()?;
        cache.pending_vectors.insert(key.clone());
    }
    Some(stand_in(&cache, &key))
}

fn stand_in(cache: &AssetPixelCache, key: &RenderedVectorKey) -> Option<Image> {
    let aspect = |width: u32, height: u32| width as f32 / height.max(1) as f32;
    let wanted = aspect(key.width, key.height);
    cache
        .vectors
        .iter()
        .filter(|(candidate, _)| {
            candidate.asset_id == key.asset_id
                && candidate.generation == key.generation
                && candidate.source_identity == key.source_identity
                && candidate.kind == key.kind
                && (aspect(candidate.width, candidate.height) / wanted - 1.0).abs()
                    <= STAND_IN_ASPECT_TOLERANCE
        })
        .min_by_key(|(candidate, _)| candidate.width.abs_diff(key.width))
        .map(|(_, variant)| variant.image.clone())
}

struct Job {
    context: crate::assets::AssetContext,
    record: Arc<crate::assets::AssetRecord>,
    tree: Arc<usvg::Tree>,
    key: RenderedVectorKey,
}

fn pool() -> &'static Sender<Job> {
    static POOL: OnceLock<Sender<Job>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = unbounded::<Job>();
        // Leave a core for the render thread; the tree and the BEAM share the rest.
        let workers = std::thread::available_parallelism()
            .map_or(1, |cores| cores.get().saturating_sub(1))
            .clamp(1, 3);
        for index in 0..workers {
            let rx = rx.clone();
            let _ = std::thread::Builder::new()
                .name(format!("emerge_svg_raster_{index}"))
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        run(job);
                    }
                });
        }
        tx
    })
}

fn run(job: Job) {
    let _context = job.context.enter();
    let started_at = Instant::now();
    let image = match job.key.kind {
        RenderedVectorVariantKind::Full => {
            rasterize_vector_tree(&job.tree, job.key.width, job.key.height)
        }
        RenderedVectorVariantKind::CoverViewport => {
            rasterize_vector_tree_cover_viewport(&job.tree, job.key.width, job.key.height)
        }
    };
    record_rasterization(
        &job.key.asset_id,
        job.key.width,
        job.key.height,
        started_at.elapsed(),
    );
    // Takes the pixel-cache lock itself, so ask before taking it.
    let current = record_is_current_for_cache(&job.record);
    let context = asset_context();
    let Ok(mut cache) = context.pixel_cache.lock() else {
        return;
    };
    if let Some(image) = image.as_ref().filter(|_| current) {
        store_vector_variant_locked(&mut cache, &job.record, job.key.clone(), image);
    }
    cache.pending_vectors.remove(&job.key);
    let notify = cache.pending_vectors.is_empty()
        || cache
            .pending_notified_at
            .is_none_or(|at| at.elapsed() >= NOTIFY_INTERVAL);
    if notify {
        cache.pending_notified_at = Some(Instant::now());
    }
    drop(cache);
    if notify {
        crate::assets::request_tree_update();
    }
}

#[cfg(test)]
thread_local! {
    /// The learned rate is process-wide; tests on other threads move it.
    static TEST_NS_PER_PIXEL: Cell<Option<f64>> = const { Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{AssetConfig, AssetRuntime};
    use crate::tree::attrs::ImageFit;

    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="300">
<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
<stop offset="0" stop-color="#ff8800"/><stop offset="1" stop-color="#2040ff"/></linearGradient></defs>
<rect width="400" height="300" fill="url(#g)"/><circle cx="200" cy="150" r="90" fill="#fff"/></svg>"##;

    fn with_rate<T>(ns_per_pixel: f64, run: impl FnOnce() -> T) -> T {
        TEST_NS_PER_PIXEL.with(|rate| rate.set(Some(ns_per_pixel)));
        let result = run();
        TEST_NS_PER_PIXEL.with(|rate| rate.set(None));
        result
    }

    fn insert(id: &str) {
        let tree = usvg::Tree::from_str(SVG, &usvg::Options::default()).unwrap();
        insert_vector_asset(id, tree).unwrap();
    }

    fn draw(id: &str, width: u32, height: u32) -> Vec<u8> {
        let mut surface =
            skia_safe::surfaces::raster_n32_premul((width as i32, height as i32)).unwrap();
        surface.canvas().clear(Color::TRANSPARENT);
        let spec = ImageDrawSpec {
            rect: RectSpec {
                x: 0.0,
                y: 0.0,
                w: width as f32,
                h: height as f32,
            },
            image_id: id,
            fit: ImageFit::Contain,
            svg_tint: None,
        };
        draw_cached_asset_with_fit(surface.canvas(), spec, 0.0);
        read(&surface.image_snapshot(), width, height)
    }

    fn read(image: &Image, width: u32, height: u32) -> Vec<u8> {
        let info = ImageInfo::new(
            (width as i32, height as i32),
            ColorType::RGBA8888,
            AlphaType::Premul,
            None,
        );
        let mut pixels = vec![0; width as usize * height as usize * 4];
        assert!(image.read_pixels(
            &info,
            &mut pixels,
            width as usize * 4,
            (0, 0),
            skia_safe::image::CachingHint::Allow
        ));
        pixels
    }

    fn reference(id: &str, width: u32, height: u32) -> Vec<u8> {
        let record = crate::assets::asset_record(id).unwrap();
        let crate::assets::AssetRecordKind::Vector(tree) = &record.kind else {
            unreachable!();
        };
        read(
            &rasterize_vector_tree(tree, width, height).unwrap(),
            width,
            height,
        )
    }

    fn blank(pixels: &[u8]) -> bool {
        pixels.iter().all(|&byte| byte == 0)
    }

    fn started() -> (
        AssetRuntime,
        crossbeam_channel::Receiver<crate::actors::TreeMsg>,
    ) {
        let runtime = AssetRuntime::new();
        let (tx, rx) = crossbeam_channel::bounded(16);
        runtime.start(tx, false);
        runtime.configure(AssetConfig::default());
        (runtime, rx)
    }

    fn await_tree_update(rx: &crossbeam_channel::Receiver<crate::actors::TreeMsg>) {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(crate::actors::TreeMsg::AssetStateChanged) => {}
            other => panic!("expected a tree update, got {other:?}"),
        }
    }

    #[test]
    fn a_large_miss_is_rasterized_off_the_frame_then_a_frame_is_requested() {
        let (runtime, rx) = started();
        let _context = runtime.enter();
        insert("art");
        let revision = crate::assets::asset_record("art")
            .unwrap()
            .render_revision
            .load(Ordering::Relaxed);

        let first = with_rate(1_000.0, || {
            let _frame = enter_frame(true);
            draw("art", 400, 300)
        });
        assert!(
            blank(&first),
            "a deferred picture draws nothing in its first frame"
        );
        await_tree_update(&rx);

        assert_eq!(rendered_vector_cache_entry_count(), 1);
        assert!(
            crate::assets::asset_record("art")
                .unwrap()
                .render_revision
                .load(Ordering::Relaxed)
                != revision,
            "storing the variant invalidates layers drawn without it"
        );
        let next = with_rate(1_000.0, || {
            let _frame = enter_frame(true);
            draw("art", 400, 300)
        });
        assert_eq!(next, reference("art", 400, 300));
        assert!(rx.try_recv().is_err(), "a hit asks for nothing");
        runtime.stop();
    }

    #[test]
    fn misses_within_the_frame_budget_are_drawn_in_the_frame() {
        let (runtime, rx) = started();
        let _context = runtime.enter();
        insert("icons");
        // 100 x 100 at 300 ns/px predicts 3 ms: the first fits the 4 ms budget,
        // the second doesn't.
        let (first, second) = with_rate(300.0, || {
            let _frame = enter_frame(true);
            (draw("icons", 100, 75), draw("icons", 120, 90))
        });
        assert_eq!(first, reference("icons", 100, 75));
        assert!(
            !blank(&second),
            "the cached 100 x 75 stands in for 120 x 90"
        );
        await_tree_update(&rx);
        assert_eq!(rendered_vector_cache_entry_count(), 2);
        // A new frame has a new budget.
        let third = with_rate(300.0, || {
            let _frame = enter_frame(true);
            draw("icons", 80, 60)
        });
        assert_eq!(third, reference("icons", 80, 60));
        runtime.stop();
    }

    #[test]
    fn without_an_opted_in_frame_or_asset_worker_misses_stay_synchronous() {
        let runtime = AssetRuntime::new();
        let _context = runtime.enter();
        runtime.configure(AssetConfig::default());
        insert("plain");
        let drawn = with_rate(1_000.0, || {
            let _frame = enter_frame(true); // opted in, but no worker to ask for a frame
            draw("plain", 400, 300)
        });
        assert_eq!(drawn, reference("plain", 400, 300));

        let (runtime, _rx) = started();
        let _context = runtime.enter();
        insert("plain");
        let drawn = with_rate(1_000.0, || draw("plain", 200, 150)); // renderer not opted in
        assert_eq!(drawn, reference("plain", 200, 150));
        runtime.stop();
    }

    #[test]
    fn the_prediction_uses_each_assets_own_rate() {
        // A flat chart is ~50x cheaper per pixel than gradient art; one shared
        // rate let the chart talk the art into the frame.
        record_rasterization("vr-test-chart", 1000, 500, Duration::from_micros(250));
        record_rasterization("vr-test-art", 400, 300, Duration::from_millis(3));
        assert!((ns_per_pixel("vr-test-chart") - 0.5).abs() < 1e-6);
        assert!((ns_per_pixel("vr-test-art") - 25.0).abs() < 1e-6);
        assert_eq!(ns_per_pixel("vr-test-unseen"), UNKNOWN_NS_PER_PIXEL);
        record_rasterization("vr-test-tiny", 32, 32, Duration::from_millis(1));
        assert_eq!(ns_per_pixel("vr-test-tiny"), UNKNOWN_NS_PER_PIXEL);
    }
}
