/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The task that handles all painting.

use buffer_map::BufferMap;
use display_list::{self, StackingContext};
use font_cache_task::FontCacheTask;
use font_context::FontContext;
use paint_context::PaintContext;

use azure::azure_hl::{SurfaceFormat, Color, DrawTarget, BackendType};
use azure::AzFloat;
use euclid::Matrix4;
use euclid::point::Point2D;
use euclid::rect::Rect;
use euclid::size::Size2D;
use layers::platform::surface::{NativeGraphicsMetadata, NativePaintingGraphicsContext};
use layers::platform::surface::NativeSurface;
use layers::layers::{BufferRequest, LayerBuffer, LayerBufferSet};
use layers;
use canvas_traits::CanvasMsg;
use msg::compositor_msg::{Epoch, FrameTreeId, LayerId, LayerKind};
use msg::compositor_msg::{LayerProperties, PaintListener, ScrollPolicy};
use msg::constellation_msg::Msg as ConstellationMsg;
use msg::constellation_msg::{ConstellationChan, Failure, PipelineId};
use msg::constellation_msg::PipelineExitType;
use profile_traits::mem::{self, Report, Reporter, ReportsChan};
use profile_traits::time::{self, profile};
use rand::{self, Rng};
use skia::SkiaGrGLNativeContextRef;
use std::borrow::ToOwned;
use std::mem as std_mem;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::collections::HashMap;
use url::Url;
use util::geometry::{Au, ZERO_POINT};
use util::opts;
use util::task::spawn_named_with_send_on_failure;
use util::task_state;
use util::task::spawn_named;

/// Information about a hardware graphics layer that layout sends to the painting task.
#[derive(Clone)]
pub struct PaintLayer {
    /// A per-pipeline ID describing this layer that should be stable across reflows.
    pub id: LayerId,
    /// The color of the background in this layer. Used for unpainted content.
    pub background_color: Color,
    /// The scrolling policy of this layer.
    pub scroll_policy: ScrollPolicy,
}

impl PaintLayer {
    /// Creates a new `PaintLayer`.
    pub fn new(id: LayerId, background_color: Color, scroll_policy: ScrollPolicy) -> PaintLayer {
        PaintLayer {
            id: id,
            background_color: background_color,
            scroll_policy: scroll_policy,
        }
    }
}

pub struct PaintRequest {
    pub buffer_requests: Vec<BufferRequest>,
    pub scale: f32,
    pub layer_id: LayerId,
    pub epoch: Epoch,
    pub layer_kind: LayerKind,
}

pub enum Msg {
    PaintInit(Epoch, Arc<StackingContext>),
    CanvasLayer(LayerId, Arc<Mutex<Sender<CanvasMsg>>>),
    Paint(Vec<PaintRequest>, FrameTreeId),
    UnusedBuffer(Vec<Box<LayerBuffer>>),
    PaintPermissionGranted,
    PaintPermissionRevoked,
    CollectReports(ReportsChan),
    Exit(Option<Sender<()>>, PipelineExitType),
}

#[derive(Clone)]
pub struct PaintChan(Sender<Msg>);

impl PaintChan {
    pub fn new() -> (Receiver<Msg>, PaintChan) {
        let (chan, port) = channel();
        (port, PaintChan(chan))
    }

    pub fn send(&self, msg: Msg) {
        assert!(self.send_opt(msg).is_ok(), "PaintChan.send: paint port closed")
    }

    pub fn send_opt(&self, msg: Msg) -> Result<(), Msg> {
        let &PaintChan(ref chan) = self;
        chan.send(msg).map_err(|e| e.0)
    }
}

impl Reporter for PaintChan {
    // Just injects an appropriate event into the paint task's queue.
    fn collect_reports(&self, reports_chan: ReportsChan) -> bool {
        let PaintChan(ref c) = *self;
        c.send(Msg::CollectReports(reports_chan)).is_ok()
    }
}

pub struct PaintTask<C> {
    id: PipelineId,
    url: Url,
    port: Receiver<Msg>,
    compositor: C,
    constellation_chan: ConstellationChan,

    /// A channel to the time profiler.
    time_profiler_chan: time::ProfilerChan,

    /// A channel to the memory profiler.
    mem_profiler_chan: mem::ProfilerChan,

    /// The name used for the task's memory reporter.
    pub reporter_name: String,

    /// The native graphics context.
    native_graphics_context: Option<NativePaintingGraphicsContext>,

    /// The root stacking context sent to us by the layout thread.
    root_stacking_context: Option<Arc<StackingContext>>,

    /// Permission to send paint messages to the compositor
    paint_permission: bool,

    /// The current epoch counter is passed by the layout task
    current_epoch: Option<Epoch>,

    /// A data structure to store unused LayerBuffers
    buffer_map: BufferMap,

    /// Communication handles to each of the worker threads.
    worker_threads: Vec<WorkerThreadProxy>,

    /// Tracks the number of buffers that the compositor currently owns. The
    /// PaintTask waits to exit until all buffers are returned.
    used_buffer_count: usize,

    /// A map to track the canvas specific layers
    canvas_map: HashMap<LayerId, Arc<Mutex<Sender<CanvasMsg>>>>,
}

// If we implement this as a function, we get borrowck errors from borrowing
// the whole PaintTask struct.
macro_rules! native_graphics_context(
    ($task:expr) => (
        $task.native_graphics_context.as_ref().expect("Need a graphics context to do painting")
    )
);

impl<C> PaintTask<C> where C: PaintListener + Send + 'static {
    pub fn create(id: PipelineId,
                  url: Url,
                  chan: PaintChan,
                  port: Receiver<Msg>,
                  compositor: C,
                  constellation_chan: ConstellationChan,
                  font_cache_task: FontCacheTask,
                  failure_msg: Failure,
                  time_profiler_chan: time::ProfilerChan,
                  mem_profiler_chan: mem::ProfilerChan,
                  shutdown_chan: Sender<()>) {
        let ConstellationChan(c) = constellation_chan.clone();
        spawn_named_with_send_on_failure(format!("PaintTask {:?}", id), task_state::PAINT, move || {
            {
                // Ensures that the paint task and graphics context are destroyed before the
                // shutdown message.
                let mut compositor = compositor;
                let native_graphics_context = compositor.graphics_metadata().map(
                    |md| NativePaintingGraphicsContext::from_metadata(&md));
                let worker_threads = WorkerThreadProxy::spawn(compositor.graphics_metadata(),
                                                              font_cache_task,
                                                              time_profiler_chan.clone());

                // Register this thread as a memory reporter, via its own channel.
                let reporter = box chan.clone();
                let reporter_name = format!("paint-reporter-{}", id.0);
                mem_profiler_chan.send(mem::ProfilerMsg::RegisterReporter(reporter_name.clone(),
                                                                          reporter));

                // FIXME: rust/#5967
                let mut paint_task = PaintTask {
                    id: id,
                    url: url,
                    port: port,
                    compositor: compositor,
                    constellation_chan: constellation_chan,
                    time_profiler_chan: time_profiler_chan,
                    mem_profiler_chan: mem_profiler_chan,
                    reporter_name: reporter_name,
                    native_graphics_context: native_graphics_context,
                    root_stacking_context: None,
                    paint_permission: false,
                    current_epoch: None,
                    buffer_map: BufferMap::new(10000000),
                    worker_threads: worker_threads,
                    used_buffer_count: 0,
                    canvas_map: HashMap::new()
                };

                paint_task.start();

                // Destroy all the buffers.
                match paint_task.native_graphics_context.as_ref() {
                    Some(ctx) => paint_task.buffer_map.clear(ctx),
                    None => (),
                }

                // Tell all the worker threads to shut down.
                for worker_thread in paint_task.worker_threads.iter_mut() {
                    worker_thread.exit()
                }
            }

            debug!("paint_task: shutdown_chan send");
            shutdown_chan.send(()).unwrap();
        }, ConstellationMsg::Failure(failure_msg), c);
    }

    fn start(&mut self) {
        debug!("PaintTask: beginning painting loop");

        let mut exit_response_channel : Option<Sender<()>> = None;
        let mut waiting_for_compositor_buffers_to_exit = false;
        loop {
            match self.port.recv().unwrap() {
                Msg::PaintInit(epoch, stacking_context) => {
                    self.current_epoch = Some(epoch);
                    self.root_stacking_context = Some(stacking_context.clone());

                    if !self.paint_permission {
                        debug!("PaintTask: paint ready msg");
                        let ConstellationChan(ref mut c) = self.constellation_chan;
                        c.send(ConstellationMsg::PainterReady(self.id)).unwrap();
                        continue;
                    }

                    // If waiting to exit, ignore any more paint commands
                    if waiting_for_compositor_buffers_to_exit {
                        continue;
                    }

                    self.initialize_layers();
                }
                // Inserts a new canvas renderer to the layer map
                Msg::CanvasLayer(layer_id, canvas_renderer) => {
                    debug!("Renderer received for canvas with layer {:?}", layer_id);
                    self.canvas_map.insert(layer_id, canvas_renderer);
                }
                Msg::Paint(requests, frame_tree_id) => {
                    if !self.paint_permission {
                        debug!("PaintTask: paint ready msg");
                        let ConstellationChan(ref mut c) = self.constellation_chan;
                        c.send(ConstellationMsg::PainterReady(self.id)).unwrap();
                        continue;
                    }

                    // If waiting to exit, ignore any more paint commands
                    if waiting_for_compositor_buffers_to_exit {
                        continue;
                    }

                    let mut replies = Vec::new();
                    for PaintRequest { buffer_requests, scale, layer_id, epoch, layer_kind }
                          in requests.into_iter() {
                        if self.current_epoch == Some(epoch) {
                            self.paint(&mut replies, buffer_requests, scale, layer_id, layer_kind);
                        } else {
                            debug!("painter epoch mismatch: {:?} != {:?}", self.current_epoch, epoch);
                        }
                    }

                    for reply in replies.iter() {
                        let &(_, ref buffer_set) = reply;
                        self.used_buffer_count += (*buffer_set).buffers.len();
                    }

                    debug!("PaintTask: returning surfaces");
                    self.compositor.assign_painted_buffers(self.id,
                                                           self.current_epoch.unwrap(),
                                                           replies,
                                                           frame_tree_id);
                }
                Msg::UnusedBuffer(unused_buffers) => {
                    debug!("PaintTask {:?}: Received {} unused buffers", self.id, unused_buffers.len());
                    self.used_buffer_count -= unused_buffers.len();

                    for buffer in unused_buffers.into_iter().rev() {
                        self.buffer_map.insert(native_graphics_context!(self), buffer);
                    }

                    if waiting_for_compositor_buffers_to_exit && self.used_buffer_count == 0 {
                        debug!("PaintTask: Received all loaned buffers, exiting.");
                        exit_response_channel.map(|channel| channel.send(()));
                        break;
                    }
                }
                Msg::PaintPermissionGranted => {
                    self.paint_permission = true;

                    if self.root_stacking_context.is_some() {
                        self.initialize_layers();
                    }
                }
                Msg::PaintPermissionRevoked => {
                    self.paint_permission = false;
                }
                Msg::CollectReports(reports_chan) => {
                    // FIXME(njn): should eventually measure other parts of the paint task.
                    let mut reports = vec![];
                    reports.push(Report {
                        path: path!["pages", format!("url({})", self.url), "paint-task", "buffer-map"],
                        size: self.buffer_map.mem(),
                    });
                    reports_chan.send(reports);
                }
                Msg::Exit(response_channel, exit_type) => {
                    let msg = mem::ProfilerMsg::UnregisterReporter(self.reporter_name.clone());
                    self.mem_profiler_chan.send(msg);

                    // Ask the compositor to return any used buffers it
                    // is holding for this paint task. This previously was
                    // sent from the constellation. However, it needs to be sent
                    // from here to avoid a race condition with the paint
                    // messages above.
                    self.compositor.notify_paint_task_exiting(self.id);

                    let should_wait_for_compositor_buffers = match exit_type {
                        PipelineExitType::Complete => false,
                        PipelineExitType::PipelineOnly => self.used_buffer_count != 0
                    };

                    if !should_wait_for_compositor_buffers {
                        debug!("PaintTask: Exiting without waiting for compositor buffers.");
                        response_channel.map(|channel| channel.send(()));
                        break;
                    }

                    // If we own buffers in the compositor and we are not exiting completely, wait
                    // for the compositor to return buffers, so that we can release them properly.
                    // When doing a complete exit, the compositor lets all buffers leak.
                    debug!("PaintTask {:?}: Saw ExitMsg, {} buffers in use", self.id, self.used_buffer_count);
                    waiting_for_compositor_buffers_to_exit = true;
                    exit_response_channel = response_channel;
                }
            }
        }
    }

    /// Retrieves an appropriately-sized layer buffer from the cache to match the requirements of
    /// the given tile, or creates one if a suitable one cannot be found.
    fn find_or_create_layer_buffer_for_tile(&mut self, tile: &BufferRequest, scale: f32)
                                            -> Option<Box<LayerBuffer>> {
        let width = tile.screen_rect.size.width;
        let height = tile.screen_rect.size.height;
        if opts::get().gpu_painting {
            return None
        }

        match self.buffer_map.find(tile.screen_rect.size) {
            Some(mut buffer) => {
                buffer.rect = tile.page_rect;
                buffer.screen_pos = tile.screen_rect;
                buffer.resolution = scale;
                buffer.native_surface.mark_wont_leak();
                buffer.painted_with_cpu = true;
                buffer.content_age = tile.content_age;
                return Some(buffer)
            }
            None => {}
        }

        // Create an empty native surface. We mark it as not leaking
        // in case it dies in transit to the compositor task.
        let mut native_surface: NativeSurface =
            layers::platform::surface::NativeSurface::new(native_graphics_context!(self),
                                                          Size2D::new(width as i32, height as i32),
                                                          width as i32 * 4);
        native_surface.mark_wont_leak();

        Some(box LayerBuffer {
            native_surface: native_surface,
            rect: tile.page_rect,
            screen_pos: tile.screen_rect,
            resolution: scale,
            stride: (width * 4) as usize,
            painted_with_cpu: true,
            content_age: tile.content_age,
        })
    }

    /// Paints one layer and places the painted tiles in `replies`.
    fn paint(&mut self,
              replies: &mut Vec<(LayerId, Box<LayerBufferSet>)>,
              mut tiles: Vec<BufferRequest>,
              scale: f32,
              layer_id: LayerId,
              layer_kind: LayerKind) {
        time::profile(time::ProfilerCategory::Painting, None, self.time_profiler_chan.clone(), || {
            // Bail out if there is no appropriate stacking context.
            let stacking_context = if let Some(ref stacking_context) = self.root_stacking_context {
                match display_list::find_stacking_context_with_layer_id(stacking_context,
                                                                        layer_id) {
                    Some(stacking_context) => stacking_context,
                    None => return,
                }
            } else {
                return
            };

            // Divide up the layer into tiles and distribute them to workers via a simple round-
            // robin strategy.
            let tiles = std_mem::replace(&mut tiles, Vec::new());
            let tile_count = tiles.len();
            for (i, tile) in tiles.into_iter().enumerate() {
                let thread_id = i % self.worker_threads.len();
                let layer_buffer = self.find_or_create_layer_buffer_for_tile(&tile, scale);
                self.worker_threads[thread_id].paint_tile(thread_id,
                                                          tile,
                                                          layer_buffer,
                                                          stacking_context.clone(),
                                                          scale,
                                                          layer_kind);
            }
            let new_buffers = (0..tile_count).map(|i| {
                let thread_id = i % self.worker_threads.len();
                self.worker_threads[thread_id].get_painted_tile_buffer()
            }).collect();

            let layer_buffer_set = box LayerBufferSet {
                buffers: new_buffers,
            };
            replies.push((layer_id, layer_buffer_set));
        })
    }

    fn initialize_layers(&mut self) {
        let root_stacking_context = match self.root_stacking_context {
            None => return,
            Some(ref root_stacking_context) => root_stacking_context,
        };

        let mut properties = Vec::new();
        build(&mut properties,
              &**root_stacking_context,
              &ZERO_POINT,
              &Matrix4::identity(),
              &Matrix4::identity(),
              None);
        self.compositor.initialize_layers_for_pipeline(self.id, properties, self.current_epoch.unwrap());

        fn build(properties: &mut Vec<LayerProperties>,
                 stacking_context: &StackingContext,
                 page_position: &Point2D<Au>,
                 transform: &Matrix4,
                 perspective: &Matrix4,
                 parent_id: Option<LayerId>) {

            let transform = transform.mul(&stacking_context.transform);
            let perspective = perspective.mul(&stacking_context.perspective);

            let (next_parent_id, page_position, transform, perspective) = match stacking_context.layer {
                Some(ref paint_layer) => {
                    // Layers start at the top left of their overflow rect, as far as the info we give to
                    // the compositor is concerned.
                    let overflow_relative_page_position = *page_position +
                                                          stacking_context.bounds.origin +
                                                          stacking_context.overflow.origin;
                    let layer_position =
                        Rect::new(Point2D::new(overflow_relative_page_position.x.to_nearest_px() as f32,
                                               overflow_relative_page_position.y.to_nearest_px() as f32),
                                  Size2D::new(stacking_context.overflow.size.width.to_nearest_px() as f32,
                                              stacking_context.overflow.size.height.to_nearest_px() as f32));

                    let establishes_3d_context = stacking_context.establishes_3d_context;

                    properties.push(LayerProperties {
                        id: paint_layer.id,
                        parent_id: parent_id,
                        rect: layer_position,
                        background_color: paint_layer.background_color,
                        scroll_policy: paint_layer.scroll_policy,
                        transform: transform,
                        perspective: perspective,
                        establishes_3d_context: establishes_3d_context,
                    });

                    // When there is a new layer, the transforms and origin
                    // are handled by the compositor.
                    (Some(paint_layer.id),
                     Point2D::zero(),
                     Matrix4::identity(),
                     Matrix4::identity())
                }
                None => {
                    (parent_id,
                     stacking_context.bounds.origin + *page_position,
                     transform,
                     perspective)
                }
            };

            for kid in stacking_context.display_list.children.iter() {
                build(properties,
                      &**kid,
                      &page_position,
                      &transform,
                      &perspective,
                      next_parent_id);
            }
        }
    }
}

struct WorkerThreadProxy {
    sender: Sender<MsgToWorkerThread>,
    receiver: Receiver<MsgFromWorkerThread>,
}

impl WorkerThreadProxy {
    fn spawn(native_graphics_metadata: Option<NativeGraphicsMetadata>,
             font_cache_task: FontCacheTask,
             time_profiler_chan: time::ProfilerChan)
             -> Vec<WorkerThreadProxy> {
        let thread_count = if opts::get().gpu_painting {
            1
        } else {
            opts::get().paint_threads
        };
        (0..thread_count).map(|_| {
            let (from_worker_sender, from_worker_receiver) = channel();
            let (to_worker_sender, to_worker_receiver) = channel();
            let native_graphics_metadata = native_graphics_metadata.clone();
            let font_cache_task = font_cache_task.clone();
            let time_profiler_chan = time_profiler_chan.clone();
            spawn_named("PaintWorker".to_owned(), move || {
                let mut worker_thread = WorkerThread::new(from_worker_sender,
                                                          to_worker_receiver,
                                                          native_graphics_metadata,
                                                          font_cache_task,
                                                          time_profiler_chan);
                worker_thread.main();
            });
            WorkerThreadProxy {
                receiver: from_worker_receiver,
                sender: to_worker_sender,
            }
        }).collect()
    }

    fn paint_tile(&mut self,
                  thread_id: usize,
                  tile: BufferRequest,
                  layer_buffer: Option<Box<LayerBuffer>>,
                  stacking_context: Arc<StackingContext>,
                  scale: f32,
                  layer_kind: LayerKind) {
        let msg = MsgToWorkerThread::PaintTile(thread_id,
                                               tile,
                                               layer_buffer,
                                               stacking_context,
                                               scale,
                                               layer_kind);
        self.sender.send(msg).unwrap()
    }

    fn get_painted_tile_buffer(&mut self) -> Box<LayerBuffer> {
        match self.receiver.recv().unwrap() {
            MsgFromWorkerThread::PaintedTile(layer_buffer) => layer_buffer,
        }
    }

    fn exit(&mut self) {
        self.sender.send(MsgToWorkerThread::Exit).unwrap()
    }
}

struct WorkerThread {
    sender: Sender<MsgFromWorkerThread>,
    receiver: Receiver<MsgToWorkerThread>,
    native_graphics_context: Option<NativePaintingGraphicsContext>,
    font_context: Box<FontContext>,
    time_profiler_sender: time::ProfilerChan,
}

impl WorkerThread {
    fn new(sender: Sender<MsgFromWorkerThread>,
           receiver: Receiver<MsgToWorkerThread>,
           native_graphics_metadata: Option<NativeGraphicsMetadata>,
           font_cache_task: FontCacheTask,
           time_profiler_sender: time::ProfilerChan)
           -> WorkerThread {
        WorkerThread {
            sender: sender,
            receiver: receiver,
            native_graphics_context: native_graphics_metadata.map(|metadata| {
                NativePaintingGraphicsContext::from_metadata(&metadata)
            }),
            font_context: box FontContext::new(font_cache_task.clone()),
            time_profiler_sender: time_profiler_sender,
        }
    }

    fn main(&mut self) {
        loop {
            match self.receiver.recv().unwrap() {
                MsgToWorkerThread::Exit => break,
                MsgToWorkerThread::PaintTile(thread_id, tile, layer_buffer, stacking_context, scale, layer_kind) => {
                    let draw_target = self.optimize_and_paint_tile(thread_id,
                                                                   &tile,
                                                                   stacking_context,
                                                                   scale,
                                                                   layer_kind);
                    let buffer = self.create_layer_buffer_for_painted_tile(&tile,
                                                                           layer_buffer,
                                                                           draw_target,
                                                                           scale);
                    self.sender.send(MsgFromWorkerThread::PaintedTile(buffer)).unwrap()
                }
            }
        }
    }

    fn optimize_and_paint_tile(&mut self,
                               thread_id: usize,
                               tile: &BufferRequest,
                               stacking_context: Arc<StackingContext>,
                               scale: f32,
                               layer_kind: LayerKind)
                               -> DrawTarget {
        let size = Size2D::new(tile.screen_rect.size.width as i32, tile.screen_rect.size.height as i32);
        let draw_target = if !opts::get().gpu_painting {
            DrawTarget::new(BackendType::Skia, size, SurfaceFormat::B8G8R8A8)
        } else {
            // FIXME(pcwalton): Cache the components of draw targets (texture color buffer,
            // paintbuffers) instead of recreating them.
            let native_graphics_context =
                native_graphics_context!(self) as *const _ as SkiaGrGLNativeContextRef;
            let draw_target = DrawTarget::new_with_fbo(BackendType::Skia,
                                                       native_graphics_context,
                                                       size,
                                                       SurfaceFormat::B8G8R8A8);

            draw_target.make_current();
            draw_target
        };

        {
            // Build the paint context.
            let mut paint_context = PaintContext {
                draw_target: draw_target.clone(),
                font_context: &mut self.font_context,
                page_rect: tile.page_rect,
                screen_rect: tile.screen_rect,
                clip_rect: None,
                transient_clip: None,
                layer_kind: layer_kind,
            };

            // Apply a translation to start at the boundaries of the stacking context, since the
            // layer's origin starts at its overflow rect's origin.
            let tile_bounds = tile.page_rect.translate(
                &Point2D::new(stacking_context.overflow.origin.x.to_f32_px(),
                              stacking_context.overflow.origin.y.to_f32_px()));

            // Apply the translation to paint the tile we want.
            let matrix = Matrix4::identity();
            let matrix = matrix.scale(scale as AzFloat, scale as AzFloat, 1.0);
            let matrix = matrix.translate(-tile_bounds.origin.x as AzFloat,
                                          -tile_bounds.origin.y as AzFloat,
                                          0.0);

            // Clear the buffer.
            paint_context.clear();

            // Draw the display list.
            time::profile(time::ProfilerCategory::PaintingPerTile,
                          None,
                          self.time_profiler_sender.clone(),
                          || {
                stacking_context.optimize_and_draw_into_context(&mut paint_context,
                                                                &tile_bounds,
                                                                &matrix,
                                                                None);
                paint_context.draw_target.flush();
                    });

            if opts::get().show_debug_parallel_paint {
                // Overlay a transparent solid color to identify the thread that
                // painted this tile.
                let color = THREAD_TINT_COLORS[thread_id % THREAD_TINT_COLORS.len()];
                paint_context.draw_solid_color(&Rect::new(Point2D::new(Au(0), Au(0)),
                                                          Size2D::new(Au::from_px(size.width),
                                                                      Au::from_px(size.height))),
                                               color);
            }
            if opts::get().paint_flashing {
                // Overlay a random transparent color.
                let color = *rand::thread_rng().choose(&THREAD_TINT_COLORS[..]).unwrap();
                paint_context.draw_solid_color(&Rect::new(Point2D::new(Au(0), Au(0)),
                                                          Size2D::new(Au::from_px(size.width),
                                                                      Au::from_px(size.height))),
                                               color);
            }
        }

        draw_target
    }

    fn create_layer_buffer_for_painted_tile(&mut self,
                                            tile: &BufferRequest,
                                            layer_buffer: Option<Box<LayerBuffer>>,
                                            draw_target: DrawTarget,
                                            scale: f32)
                                            -> Box<LayerBuffer> {
        // Extract the texture from the draw target and place it into its slot in the buffer. If
        // using CPU painting, upload it first.
        //
        // FIXME(pcwalton): We should supply the texture and native surface *to* the draw target in
        // GPU painting mode, so that it doesn't have to recreate it.
        if !opts::get().gpu_painting {
            let mut buffer = layer_buffer.unwrap();
            draw_target.snapshot().get_data_surface().with_data(|data| {
                buffer.native_surface.upload(native_graphics_context!(self), data);
                debug!("painting worker thread uploading to native surface {}",
                       buffer.native_surface.get_id());
            });
            return buffer
        }

        // GPU painting path:
        draw_target.make_current();

        // We mark the native surface as not leaking in case the surfaces
        // die on their way to the compositor task.
        let mut native_surface: NativeSurface =
            NativeSurface::from_draw_target_backing(draw_target.steal_draw_target_backing());
        native_surface.mark_wont_leak();

        box LayerBuffer {
            native_surface: native_surface,
            rect: tile.page_rect,
            screen_pos: tile.screen_rect,
            resolution: scale,
            stride: (tile.screen_rect.size.width * 4),
            painted_with_cpu: false,
            content_age: tile.content_age,
        }
    }
}

enum MsgToWorkerThread {
    Exit,
    PaintTile(usize, BufferRequest, Option<Box<LayerBuffer>>, Arc<StackingContext>, f32, LayerKind),
}

enum MsgFromWorkerThread {
    PaintedTile(Box<LayerBuffer>),
}

pub static THREAD_TINT_COLORS: [Color; 8] = [
    Color { r: 6.0/255.0, g: 153.0/255.0, b: 198.0/255.0, a: 0.7 },
    Color { r: 255.0/255.0, g: 212.0/255.0, b: 83.0/255.0, a: 0.7 },
    Color { r: 116.0/255.0, g: 29.0/255.0, b: 109.0/255.0, a: 0.7 },
    Color { r: 204.0/255.0, g: 158.0/255.0, b: 199.0/255.0, a: 0.7 },
    Color { r: 242.0/255.0, g: 46.0/255.0, b: 121.0/255.0, a: 0.7 },
    Color { r: 116.0/255.0, g: 203.0/255.0, b: 196.0/255.0, a: 0.7 },
    Color { r: 255.0/255.0, g: 249.0/255.0, b: 201.0/255.0, a: 0.7 },
    Color { r: 137.0/255.0, g: 196.0/255.0, b: 78.0/255.0, a: 0.7 },
];
