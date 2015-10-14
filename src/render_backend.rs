use app_units::Au;
use clipper;
use device::{ProgramId, TextureId};
use euclid::{Rect, Point2D, Size2D, Matrix4};
use font::{FontContext, RasterizedGlyph};
use fnv::FnvHasher;
use internal_types::{ApiMsg, Frame, ImageResource, ResultMsg, DrawLayer, BatchUpdateList, BatchId, BatchUpdate, BatchUpdateOp};
use internal_types::{PackedVertex, WorkVertex, DisplayList, DrawCommand, DrawCommandInfo};
use internal_types::{CompositeInfo, BorderEdgeDirection, RenderTargetIndex, GlyphKey};
use renderer::BLUR_INFLATION_FACTOR;
use resource_list::ResourceList;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::collections::hash_state::DefaultState;
use std::cmp::Ordering;
use std::f32;
use std::mem;
use std::sync::atomic::{AtomicUsize, ATOMIC_USIZE_INIT};
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;
use std::sync::mpsc::{Sender, Receiver};
use std::thread;
use string_cache::Atom;
use texture_cache::{TextureCache, TextureCacheItem, TextureInsertOp};
use types::{DisplayListID, Epoch, BorderDisplayItem, BorderRadiusRasterOp};
use types::{BoxShadowCornerRasterOp, RectangleDisplayItem};
use types::{Glyph, GradientStop, DisplayListMode, RasterItem, ClipRegion};
use types::{GlyphInstance, ImageID, DrawList, ImageFormat, BoxShadowClipMode, DisplayItem};
use types::{PipelineId, RenderNotifier, StackingContext, SpecificDisplayItem, ColorF, DrawListID};
use types::{RenderTargetID, MixBlendMode, CompositeDisplayItem, BorderSide, BorderStyle, NodeIndex};
use util;
use util::MatrixHelpers;
use scoped_threadpool;

type DisplayListMap = HashMap<DisplayListID, DisplayList, DefaultState<FnvHasher>>;
type DrawListMap = HashMap<DrawListID, DrawList, DefaultState<FnvHasher>>;
type FlatDrawListArray = Vec<FlatDrawList>;
type GlyphToImageMap = HashMap<GlyphKey, ImageID, DefaultState<FnvHasher>>;
type RasterToImageMap = HashMap<RasterItem, ImageID, DefaultState<FnvHasher>>;
type FontTemplateMap = HashMap<Atom, FontTemplate, DefaultState<FnvHasher>>;
type ImageTemplateMap = HashMap<ImageID, ImageResource, DefaultState<FnvHasher>>;
type StackingContextMap = HashMap<PipelineId, RootStackingContext, DefaultState<FnvHasher>>;

const MAX_MATRICES_PER_BATCH: usize = 32;

static FONT_CONTEXT_COUNT: AtomicUsize = ATOMIC_USIZE_INIT;

thread_local!(pub static FONT_CONTEXT: RefCell<FontContext> = RefCell::new(FontContext::new()));

struct RenderBatch {
    batch_id: BatchId,
    sort_key: DisplayItemKey,
    program_id: ProgramId,
    color_texture_id: TextureId,
    mask_texture_id: TextureId,
    vertices: Vec<PackedVertex>,
    indices: Vec<u16>,
    matrix_map: HashMap<DrawListIndex, u8>,
}

static MAX_RECT: Rect<f32> = Rect {
    origin: Point2D {
        x: -1000.0,
        y: -1000.0,
    },
    size: Size2D {
        width: 10000.0,
        height: 10000.0,
    },
};

const BORDER_DASH_SIZE: f32 = 3.0;

#[derive(Debug)]
struct RenderTarget {
    size: Size2D<u32>,
    draw_list_indices: Vec<DrawListIndex>,
    texture_id: Option<TextureId>,
}

impl RenderTarget {
    fn new(size: Size2D<u32>, texture_id: Option<TextureId>) -> RenderTarget {
        RenderTarget {
            size: size,
            draw_list_indices: Vec::new(),
            texture_id: texture_id,
        }
    }
}

struct DisplayItemIterator<'a> {
    flat_draw_lists: &'a FlatDrawListArray,
    current_key: DisplayItemKey,
    last_key: DisplayItemKey,
}

impl<'a> DisplayItemIterator<'a> {
    fn new(flat_draw_lists: &'a FlatDrawListArray,
           src_items: &Vec<DisplayItemKey>) -> DisplayItemIterator<'a> {

        match (src_items.first(), src_items.last()) {
            (Some(first), Some(last)) => {
                let current_key = first.clone();
                let mut last_key = last.clone();

                let DrawListItemIndex(last_item_index) = last_key.item_index;
                last_key.item_index = DrawListItemIndex(last_item_index + 1);

                DisplayItemIterator {
                    current_key: current_key,
                    last_key: last_key,
                    flat_draw_lists: flat_draw_lists,
                }
            }
            (None, None) => {
                DisplayItemIterator {
                    current_key: DisplayItemKey::new(0, 0),
                    last_key: DisplayItemKey::new(0, 0),
                    flat_draw_lists: flat_draw_lists
                }
            }
            _ => unreachable!(),
        }
    }
}

impl<'a> Iterator for DisplayItemIterator<'a> {
    type Item = DisplayItemKey;

    fn next(&mut self) -> Option<DisplayItemKey> {
        if self.current_key == self.last_key {
            return None;
        }

        let key = self.current_key.clone();
        let DrawListItemIndex(item_index) = key.item_index;
        let DrawListIndex(list_index) = key.draw_list_index;

        self.current_key.item_index = DrawListItemIndex(item_index + 1);

        if key.draw_list_index != self.last_key.draw_list_index {
            let last_item_index = DrawListItemIndex(self.flat_draw_lists[list_index as usize].draw_list.items.len() as u32);
            if self.current_key.item_index == last_item_index {
                self.current_key.draw_list_index = DrawListIndex(list_index + 1);
                self.current_key.item_index = DrawListItemIndex(0);
            }
        }

        Some(key)
    }
}

trait GetDisplayItemHelper {
    fn get_item(&self, key: &DisplayItemKey) -> &DisplayItem;
    fn get_item_and_draw_context(&self, key: &DisplayItemKey) -> (&DisplayItem, &DrawContext);
}

impl GetDisplayItemHelper for FlatDrawListArray {
    fn get_item(&self, key: &DisplayItemKey) -> &DisplayItem {
        let DrawListIndex(list_index) = key.draw_list_index;
        let DrawListItemIndex(item_index) = key.item_index;
        &self[list_index as usize].draw_list.items[item_index as usize]
    }

    fn get_item_and_draw_context(&self, key: &DisplayItemKey) -> (&DisplayItem, &DrawContext) {
        let DrawListIndex(list_index) = key.draw_list_index;
        let DrawListItemIndex(item_index) = key.item_index;
        //println!("\tget_item_and_draw_context list={} item={} lists_len={}", list_index, item_index, self.len());
        let list = &self[list_index as usize];
        //println!("list {} has {} items", list_index, list.draw_list.items.len());
        (&list.draw_list.items[item_index as usize], &list.draw_context)
    }
}

trait StackingContextHelpers {
    fn needs_render_target(&self) -> bool;
}

impl StackingContextHelpers for StackingContext {
    fn needs_render_target(&self) -> bool {
        match self.mix_blend_mode {
            MixBlendMode::Normal => false,
            MixBlendMode::Multiply |
            MixBlendMode::Screen |
            MixBlendMode::Overlay |
            MixBlendMode::Darken |
            MixBlendMode::Lighten |
            MixBlendMode::ColorDodge |
            MixBlendMode::ColorBurn |
            MixBlendMode::HardLight |
            MixBlendMode::SoftLight |
            MixBlendMode::Difference |
            MixBlendMode::Exclusion |
            MixBlendMode::Hue |
            MixBlendMode::Saturation |
            MixBlendMode::Color |
            MixBlendMode::Luminosity => true,
        }
    }
}

#[derive(Clone)]
struct DrawContext {
    render_target_index: RenderTargetIndex,
    overflow: Rect<f32>,
    device_pixel_ratio: f32,
    final_transform: Matrix4,
}

struct FlatDrawList {
    pub id: Option<DrawListID>,
    pub draw_context: DrawContext,
    pub draw_list: DrawList,
}

struct StackingContextDrawLists {
    background_and_borders: Vec<DrawListID>,
    block_background_and_borders: Vec<DrawListID>,
    floats: Vec<DrawListID>,
    content: Vec<DrawListID>,
    positioned_content: Vec<DrawListID>,
    outlines: Vec<DrawListID>,
}

impl StackingContextDrawLists {
    fn new() -> StackingContextDrawLists {
        StackingContextDrawLists {
            background_and_borders: Vec::new(),
            block_background_and_borders: Vec::new(),
            floats: Vec::new(),
            content: Vec::new(),
            positioned_content: Vec::new(),
            outlines: Vec::new(),
        }
    }

    #[inline(always)]
    fn push_draw_list_id(id: Option<DrawListID>, list: &mut Vec<DrawListID>) {
        if let Some(id) = id {
            list.push(id);
        }
    }
}

trait CollectDrawListsForStackingContext {
    fn collect_draw_lists(&self, display_lists: &DisplayListMap) -> StackingContextDrawLists;
}

impl CollectDrawListsForStackingContext for StackingContext {
    fn collect_draw_lists(&self, display_lists: &DisplayListMap) -> StackingContextDrawLists {
        let mut result = StackingContextDrawLists::new();

        for display_list_id in &self.display_lists {
            let display_list = &display_lists[display_list_id];
            match display_list.mode {
                DisplayListMode::Default => {
                    StackingContextDrawLists::push_draw_list_id(display_list.background_and_borders_id,
                                                                &mut result.background_and_borders);
                    StackingContextDrawLists::push_draw_list_id(display_list.block_backgrounds_and_borders_id,
                                                                &mut result.block_background_and_borders);
                    StackingContextDrawLists::push_draw_list_id(display_list.floats_id,
                                                                &mut result.floats);
                    StackingContextDrawLists::push_draw_list_id(display_list.content_id,
                                                                &mut result.content);
                    StackingContextDrawLists::push_draw_list_id(display_list.positioned_content_id,
                                                                &mut result.positioned_content);
                    StackingContextDrawLists::push_draw_list_id(display_list.outlines_id,
                                                                &mut result.outlines);
                }
                DisplayListMode::PseudoFloat => {
                    StackingContextDrawLists::push_draw_list_id(display_list.background_and_borders_id,
                                                                &mut result.floats);
                    StackingContextDrawLists::push_draw_list_id(display_list.block_backgrounds_and_borders_id,
                                                                &mut result.floats);
                    StackingContextDrawLists::push_draw_list_id(display_list.floats_id,
                                                                &mut result.floats);
                    StackingContextDrawLists::push_draw_list_id(display_list.content_id,
                                                                &mut result.floats);
                    StackingContextDrawLists::push_draw_list_id(display_list.positioned_content_id,
                                                                &mut result.floats);
                    StackingContextDrawLists::push_draw_list_id(display_list.outlines_id,
                                                                &mut result.floats);
                }
                DisplayListMode::PseudoPositionedContent => {
                    StackingContextDrawLists::push_draw_list_id(display_list.background_and_borders_id,
                                                                &mut result.positioned_content);
                    StackingContextDrawLists::push_draw_list_id(display_list.block_backgrounds_and_borders_id,
                                                                &mut result.positioned_content);
                    StackingContextDrawLists::push_draw_list_id(display_list.floats_id,
                                                                &mut result.positioned_content);
                    StackingContextDrawLists::push_draw_list_id(display_list.content_id,
                                                                &mut result.positioned_content);
                    StackingContextDrawLists::push_draw_list_id(display_list.positioned_content_id,
                                                                &mut result.positioned_content);
                    StackingContextDrawLists::push_draw_list_id(display_list.outlines_id,
                                                                &mut result.positioned_content);
                }
            }
        }

        result
    }
}

#[derive(Clone, Copy, Debug, Ord, PartialOrd, PartialEq, Eq, Hash)]
struct DrawListIndex(u32);

#[derive(Clone, Copy, Debug, Ord, PartialOrd, PartialEq, Eq)]
struct DrawListItemIndex(u32);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayItemKey {
    draw_list_index: DrawListIndex,
    item_index: DrawListItemIndex,
}

impl DisplayItemKey {
    fn new(draw_list_index: usize, item_index: usize) -> DisplayItemKey {
        DisplayItemKey {
            draw_list_index: DrawListIndex(draw_list_index as u32),
            item_index: DrawListItemIndex(item_index as u32),
        }
    }
}

struct Scene {
    pipeline_epoch_map: HashMap<PipelineId, Epoch>,
    aabb_tree: AABBTree,
    flat_draw_lists: Vec<FlatDrawList>,
    thread_pool: scoped_threadpool::Pool,
    scroll_offset: Point2D<f32>,

    render_targets: Vec<RenderTarget>,
    render_target_stack: Vec<RenderTargetIndex>,

    pending_updates: BatchUpdateList,
}

impl Scene {
    fn new() -> Scene {
        Scene {
            pipeline_epoch_map: HashMap::new(),
            aabb_tree: AABBTree::new(512.0),
            flat_draw_lists: Vec::new(),
            thread_pool: scoped_threadpool::Pool::new(8),
            scroll_offset: Point2D::zero(),
            render_targets: Vec::new(),
            render_target_stack: Vec::new(),
            pending_updates: BatchUpdateList::new(),
        }
    }

    pub fn pending_updates(&mut self) -> BatchUpdateList {
        mem::replace(&mut self.pending_updates, BatchUpdateList::new())
    }

    fn reset(&mut self, texture_cache: &mut TextureCache) {
        debug_assert!(self.render_target_stack.len() == 0);
        self.pipeline_epoch_map.clear();

        for node in &mut self.aabb_tree.nodes {
            if let Some(ref compiled_node) = node.compiled_node {
                for batch in &compiled_node.batches {
                    self.pending_updates.push(BatchUpdate {
                        id: batch.batch_id,
                        op: BatchUpdateOp::Destroy,
                    });
                }
            }
        }

        // Free any render targets from last frame.
        // TODO: This should really re-use existing targets here...
        for render_target in &mut self.render_targets {
            if let Some(texture_id) = render_target.texture_id {
                texture_cache.free_render_target(texture_id);
            }
        }

        self.render_targets.clear();
    }

    fn push_render_target(&mut self,
                          size: Size2D<u32>,
                          texture_id: Option<TextureId>) {
        let rt_index = RenderTargetIndex(self.render_targets.len() as u32);
        self.render_target_stack.push(rt_index);

        let render_target = RenderTarget::new(size, texture_id);
        self.render_targets.push(render_target);
    }

    fn current_render_target(&self) -> RenderTargetIndex {
        *self.render_target_stack.last().unwrap()
    }

    fn pop_render_target(&mut self) {
        self.render_target_stack.pop().unwrap();
    }

    fn push_draw_list(&mut self,
                      id: Option<DrawListID>,
                      draw_list: DrawList,
                      draw_context: &DrawContext) {
        let RenderTargetIndex(current_render_target) = *self.render_target_stack.last().unwrap();
        let render_target = &mut self.render_targets[current_render_target as usize];

        let draw_list_index = DrawListIndex(self.flat_draw_lists.len() as u32);
        render_target.draw_list_indices.push(draw_list_index);

        self.flat_draw_lists.push(FlatDrawList {
            id: id,
            draw_context: draw_context.clone(),
            draw_list: draw_list,
        });
    }

    fn add_draw_list(&mut self,
                     draw_list_id: DrawListID,
                     draw_context: &DrawContext,
                     draw_list_map: &mut DrawListMap,
                     iframes: &mut Vec<IframeInfo>) {
        let draw_list = draw_list_map.remove(&draw_list_id).expect(&format!("unable to remove draw list {:?}", draw_list_id));

        // TODO: DrawList should set a flag if iframes are added, to avoid this loop in the common case of no iframes.
        for item in &draw_list.items {
            match item.item {
                SpecificDisplayItem::Iframe(ref info) => {
                    let iframe_offset = draw_context.final_transform.transform_point(&item.rect.origin);
                    iframes.push(IframeInfo::new(info.iframe, iframe_offset));
                }
                _ => {}
            }
        }

        self.push_draw_list(Some(draw_list_id),
                            draw_list,
                            draw_context);
    }

    fn flatten_stacking_context(&mut self,
                                stacking_context_kind: StackingContextKind,
                                transform: &Matrix4,
                                display_list_map: &DisplayListMap,
                                draw_list_map: &mut DrawListMap,
                                stacking_contexts: &StackingContextMap,
                                device_pixel_ratio: f32,
                                texture_cache: &mut TextureCache) {
        let _pf = util::ProfileScope::new("  flatten_stacking_context");
        let stacking_context = match stacking_context_kind {
            StackingContextKind::Normal(stacking_context) => stacking_context,
            StackingContextKind::Root(root) => &root.stacking_context,
        };

        let mut iframes = Vec::new();

        let mut transform = transform.translate(stacking_context.bounds.origin.x,
                                                stacking_context.bounds.origin.y,
                                                0.0);

        let mut draw_context = DrawContext {
            render_target_index: self.current_render_target(),
            overflow: stacking_context.overflow,
            device_pixel_ratio: device_pixel_ratio,
            final_transform: transform,
        };

        let needs_render_target = stacking_context.needs_render_target();
        if needs_render_target {
            let size = Size2D::new(stacking_context.overflow.size.width as u32,
                                   stacking_context.overflow.size.height as u32);
            let texture_id = texture_cache.allocate_render_target(size.width, size.height, ImageFormat::RGBA8);
            let TextureId(render_target_id) = texture_id;

            let mut composite_draw_list = DrawList::new();
            let composite_item = CompositeDisplayItem {
                blend_mode: stacking_context.mix_blend_mode,
                texture_id: RenderTargetID(render_target_id),
            };
            let clip = ClipRegion {
                main: stacking_context.overflow,
                complex: vec![],
            };
            let composite_item = DisplayItem {
                item: SpecificDisplayItem::Composite(composite_item),
                rect: stacking_context.overflow,
                clip: clip,
                node_index: None,
            };
            composite_draw_list.push(composite_item);
            self.push_draw_list(None, composite_draw_list, &draw_context);

            self.push_render_target(size, Some(texture_id));

            transform = Matrix4::identity();
            draw_context.final_transform = transform;
            draw_context.render_target_index = self.current_render_target();
        }

        match stacking_context_kind {
            StackingContextKind::Normal(..) => {}
            StackingContextKind::Root(root) => {
                self.pipeline_epoch_map.insert(root.pipeline_id, root.epoch);

                if root.background_color.a > 0.0 {
                    let mut root_draw_list = DrawList::new();
                    let rectangle_item = RectangleDisplayItem {
                        color: root.background_color.clone(),
                    };
                    let clip = ClipRegion {
                        main: stacking_context.overflow,
                        complex: vec![],
                    };
                    let root_bg_color_item = DisplayItem {
                        item: SpecificDisplayItem::Rectangle(rectangle_item),
                        rect: stacking_context.overflow,
                        clip: clip,
                        node_index: None,
                    };
                    root_draw_list.push(root_bg_color_item);

                    self.push_draw_list(None, root_draw_list, &draw_context);
                }
            }
        }

        let draw_list_ids = stacking_context.collect_draw_lists(display_list_map);

        for id in &draw_list_ids.background_and_borders {
            self.add_draw_list(*id, &draw_context, draw_list_map, &mut iframes);
        }

        // TODO: Sort children (or store in two arrays) to avoid having
        //       to iterate this list twice.
        for child in &stacking_context.children {
            if child.z_index >= 0 {
                continue;
            }
            self.flatten_stacking_context(StackingContextKind::Normal(child),
                                          &transform,
                                          display_list_map,
                                          draw_list_map,
                                          stacking_contexts,
                                          device_pixel_ratio,
                                          texture_cache);
        }

        for id in &draw_list_ids.block_background_and_borders {
            self.add_draw_list(*id, &draw_context, draw_list_map, &mut iframes);
        }

        for id in &draw_list_ids.floats {
            self.add_draw_list(*id, &draw_context, draw_list_map, &mut iframes);
        }

        for id in &draw_list_ids.content {
            self.add_draw_list(*id, &draw_context, draw_list_map, &mut iframes);
        }

        for id in &draw_list_ids.positioned_content {
            self.add_draw_list(*id, &draw_context, draw_list_map, &mut iframes);
        }

        for child in &stacking_context.children {
            if child.z_index < 0 {
                continue;
            }
            self.flatten_stacking_context(StackingContextKind::Normal(child),
                                          &transform,
                                          display_list_map,
                                          draw_list_map,
                                          stacking_contexts,
                                          device_pixel_ratio,
                                          texture_cache);
        }

        // TODO: This ordering isn't quite right - it should look
        //       at the z-index in the iframe root stacking context.
        for iframe_info in &iframes {
            let iframe = stacking_contexts.get(&iframe_info.id);
            if let Some(iframe) = iframe {
                // TODO: DOesn't handle transforms on iframes yet!
                let iframe_transform = Matrix4::identity().translate(iframe_info.offset.x,
                                                                     iframe_info.offset.y,
                                                                     0.0);
                self.flatten_stacking_context(StackingContextKind::Root(iframe),
                                              &iframe_transform,
                                              display_list_map,
                                              draw_list_map,
                                              stacking_contexts,
                                              device_pixel_ratio,
                                              texture_cache);
            }
        }

        for id in &draw_list_ids.outlines {
            self.add_draw_list(*id, &draw_context, draw_list_map, &mut iframes);
        }

        if needs_render_target {
            self.pop_render_target();
        }
    }

    fn build_aabb_tree(&mut self, scene_rect: &Rect<f32>) {
        let _pf = util::ProfileScope::new("  build_aabb_tree");
        self.aabb_tree.init(scene_rect);

        // push all visible draw lists into aabb tree
        for (draw_list_index, flat_draw_list) in self.flat_draw_lists.iter_mut().enumerate() {
            for (item_index, item) in flat_draw_list.draw_list.items.iter_mut().enumerate() {
                assert!(item.node_index.is_none());
                let rect = flat_draw_list.draw_context.final_transform.transform_rect(&item.rect);
                item.node_index = self.aabb_tree.insert(&rect, draw_list_index, item_index);
            }
        }

        //self.aabb_tree.print(0, 0);
    }

    fn build_frame(&mut self,
                   viewport: &Rect<i32>,
                   device_pixel_ratio: f32,
                   raster_to_image_map: &mut RasterToImageMap,
                   glyph_to_image_map: &mut GlyphToImageMap,
                   image_templates: &ImageTemplateMap,
                   font_templates: &FontTemplateMap,
                   texture_cache: &mut TextureCache,
                   white_image_id: ImageID,
                   dummy_mask_image_id: ImageID,
                   quad_program_id: ProgramId,
                   glyph_program_id: ProgramId) -> Frame {
        let origin = Point2D::new(viewport.origin.x as f32, viewport.origin.y as f32);
        let size = Size2D::new(viewport.size.width as f32, viewport.size.height as f32);
        let viewport_rect = Rect::new(origin, size);

        // Traverse tree to calculate visible nodes
        let adjusted_viewport = viewport_rect.translate(&-self.scroll_offset);
        self.aabb_tree.cull(&adjusted_viewport);

        // Build resource list for newly visible nodes
        self.update_resource_lists();

        // Update texture cache and build list of raster jobs.
        let raster_jobs = self.update_texture_cache_and_build_raster_jobs(raster_to_image_map,
                                                                          glyph_to_image_map,
                                                                          image_templates,
                                                                          texture_cache);

        // Rasterize needed glyphs on worker threads
        self.raster_glyphs(raster_jobs,
                           font_templates,
                           texture_cache,
                           device_pixel_ratio);

        // Compile nodes that have become visible
        self.compile_visible_nodes(glyph_to_image_map,
                                   raster_to_image_map,
                                   texture_cache,
                                   white_image_id,
                                   dummy_mask_image_id,
                                   quad_program_id,
                                   glyph_program_id,
                                   device_pixel_ratio);

        // Update the batch cache from newly compiled nodes
        self.update_batch_cache();

        // Collect the visible batches into a frame
        self.collect_and_sort_visible_batches()
    }

    fn collect_and_sort_visible_batches(&mut self) -> Frame {
        let mut frame = Frame::new(self.pipeline_epoch_map.clone());

        let mut layers = Vec::new();

        for render_target in &self.render_targets {
            layers.push(DrawLayer::new(render_target.texture_id,
                                       render_target.size,
                                       Vec::new()));
        }

        for node in &self.aabb_tree.nodes {
            if node.is_visible {
                debug_assert!(node.compiled_node.is_some());
                let compiled_node = node.compiled_node.as_ref().unwrap();

                // Update batch matrices
                for (batch_id, matrix_map) in &compiled_node.matrix_maps {
                    // TODO: Could cache these matrices rather than generate for every batch.
                    let mut matrix_palette = vec![Matrix4::identity(); matrix_map.len()];

                    for (draw_list_index, matrix_index) in matrix_map {
                        let DrawListIndex(draw_list_index) = *draw_list_index;
                        let transform = self.flat_draw_lists[draw_list_index as usize].draw_context.final_transform;
                        let transform = transform.translate(self.scroll_offset.x,
                                                            self.scroll_offset.y,
                                                            0.0);
                        let matrix_index = *matrix_index as usize;
                        matrix_palette[matrix_index] = transform;
                    }

                    self.pending_updates.push(BatchUpdate {
                        id: *batch_id,
                        op: BatchUpdateOp::UpdateUniforms(matrix_palette),
                    });
                }

                for command in &compiled_node.commands {
                    let RenderTargetIndex(render_target) = command.render_target;
                    layers[render_target as usize].commands.push(command.clone());
                }
            }
        }

        for mut layer in layers {
            if layer.commands.len() > 0 {
                layer.commands.sort_by(|a, b| {
                    let draw_list_order = a.sort_key.draw_list_index.cmp(&b.sort_key.draw_list_index);
                    match draw_list_order {
                        Ordering::Equal => {
                            a.sort_key.item_index.cmp(&b.sort_key.item_index)
                        }
                        order => {
                            order
                        }
                    }
                });

                frame.add_layer(layer);
            }
        }

        frame
    }

    fn compile_visible_nodes(&mut self,
                             glyph_to_image_map: &GlyphToImageMap,
                             raster_to_image_map: &RasterToImageMap,
                             texture_cache: &TextureCache,
                             white_image_id: ImageID,
                             dummy_mask_image_id: ImageID,
                             quad_program_id: ProgramId,
                             glyph_program_id: ProgramId,
                             device_pixel_ratio: f32) {
        let _pf = util::ProfileScope::new("  compile_visible_nodes");
        let node_rects = &self.aabb_tree.node_rects();
        let nodes = &mut self.aabb_tree.nodes;
        let flat_draw_list_array = &self.flat_draw_lists;
        let white_image_info = texture_cache.get(white_image_id);
        let mask_image_info = texture_cache.get(dummy_mask_image_id);

        self.thread_pool.scoped(|scope| {
            for node in nodes {
                if node.is_visible && node.compiled_node.is_none() {
                    scope.execute(move || {
                        node.compile(flat_draw_list_array,
                                     white_image_info,
                                     mask_image_info,
                                     glyph_to_image_map,
                                     raster_to_image_map,
                                     texture_cache,
                                     node_rects,
                                     quad_program_id,
                                     glyph_program_id,
                                     device_pixel_ratio);
                    });
                }
            }
        });
    }

    fn update_batch_cache(&mut self) {
        // Allocate and update VAOs
        for node in &mut self.aabb_tree.nodes {
            if node.is_visible {
                let compiled_node = node.compiled_node.as_mut().unwrap();
                for batch in compiled_node.batches.drain(..) {
                    self.pending_updates.push(BatchUpdate {
                        id: batch.batch_id,
                        op: BatchUpdateOp::Create(batch.vertices,
                                                  batch.indices,
                                                  batch.program_id,
                                                  batch.color_texture_id,
                                                  batch.mask_texture_id),
                    });
                    compiled_node.matrix_maps.insert(batch.batch_id, batch.matrix_map);
                }
            }
        }
    }

    fn update_texture_cache_and_build_raster_jobs(&mut self,
                                                  raster_to_image_map: &mut RasterToImageMap,
                                                  glyph_to_image_map: &mut GlyphToImageMap,
                                                  image_templates: &ImageTemplateMap,
                                                  texture_cache: &mut TextureCache) -> Vec<GlyphRasterJob> {
        let _pf = util::ProfileScope::new("  update_texture_cache_and_build_raster_jobs");

        let mut raster_jobs = Vec::new();

        for node in &self.aabb_tree.nodes {
            if node.is_visible {
                let resource_list = node.resource_list.as_ref().unwrap();

                // Update texture cache with any GPU generated procedural items.
                resource_list.for_each_raster(|raster_item| {
                    if !raster_to_image_map.contains_key(raster_item) {
                        let image_id = ImageID::new();
                        texture_cache.insert_raster_op(image_id, raster_item);
                        raster_to_image_map.insert(raster_item.clone(), image_id);
                    }
                });

                // Update texture cache with any images that aren't yet uploaded to GPU.
                resource_list.for_each_image(|image_id| {
                    if !texture_cache.exists(image_id) {
                        let image_template = image_templates.get(&image_id).expect("TODO: image not available yet! ");
                        // TODO: Can we avoid the clone of the bytes here?
                        texture_cache.insert(image_id,
                                             0,
                                             0,
                                             image_template.width,
                                             image_template.height,
                                             image_template.format,
                                             TextureInsertOp::Blit(image_template.bytes.clone()));
                    }
                });

                // Update texture cache with any newly rasterized glyphs.
                resource_list.for_each_glyph(|glyph_key| {
                    if !glyph_to_image_map.contains_key(&glyph_key) {
                        let image_id = ImageID::new();
                        raster_jobs.push(GlyphRasterJob {
                            image_id: image_id,
                            glyph_key: glyph_key.clone(),
                            result: None,
                        });
                        glyph_to_image_map.insert(glyph_key.clone(), image_id);
                    }
                });
            }
        }

        raster_jobs
    }

    fn raster_glyphs(&mut self,
                     mut jobs: Vec<GlyphRasterJob>,
                     font_templates: &FontTemplateMap,
                     texture_cache: &mut TextureCache,
                     device_pixel_ratio: f32) {
        let _pf = util::ProfileScope::new("  raster_glyphs");

        // Run raster jobs in parallel
        self.thread_pool.scoped(|scope| {
            for job in &mut jobs {
                scope.execute(move || {
                    FONT_CONTEXT.with(|font_context| {
                        let mut font_context = font_context.borrow_mut();
                        let font_template = &font_templates[&job.glyph_key.font_id];
                        font_context.add_font(&job.glyph_key.font_id, &font_template.bytes);
                        job.result = font_context.get_glyph(&job.glyph_key.font_id,
                                                            job.glyph_key.size,
                                                            job.glyph_key.index,
                                                            device_pixel_ratio);
                    });
                });
            }
        });

        // Add completed raster jobs to the texture cache
        for job in jobs {
            let result = job.result.expect("Failed to rasterize the glyph?");
            let texture_width;
            let texture_height;
            let insert_op;
            match job.glyph_key.blur_radius {
                Au(0) => {
                    texture_width = result.width;
                    texture_height = result.height;
                    insert_op = TextureInsertOp::Blit(result.bytes);
                }
                blur_radius => {
                    let blur_radius_px = f32::ceil(blur_radius.to_f32_px() * device_pixel_ratio)
                        as u32;
                    texture_width = result.width + blur_radius_px * BLUR_INFLATION_FACTOR;
                    texture_height = result.height + blur_radius_px * BLUR_INFLATION_FACTOR;
                    insert_op = TextureInsertOp::Blur(result.bytes,
                                                      Size2D::new(result.width, result.height),
                                                      blur_radius);
                }
            }
            texture_cache.insert(job.image_id,
                                 result.left,
                                 result.top,
                                 texture_width,
                                 texture_height,
                                 ImageFormat::A8,
                                 insert_op);
        }
    }

    fn update_resource_lists(&mut self) {
        let _pf = util::ProfileScope::new("  update_resource_lists");

        let flat_draw_lists = &self.flat_draw_lists;
        let nodes = &mut self.aabb_tree.nodes;

        self.thread_pool.scoped(|scope| {
            for node in nodes {
                if node.is_visible && node.compiled_node.is_none() {
                    scope.execute(move || {
                        node.build_resource_list(flat_draw_lists);
                    });
                }
            }
        });
    }

    fn scroll(&mut self, delta: Point2D<f32>) {
        self.scroll_offset = self.scroll_offset + delta;

        self.scroll_offset.x = self.scroll_offset.x.min(0.0);
        self.scroll_offset.y = self.scroll_offset.y.min(0.0);

        // TODO: Clamp end of scroll (need overflow rect + screen rect)
    }
}

struct FontTemplate {
    bytes: Arc<Vec<u8>>,
}

struct GlyphRasterJob {
    image_id: ImageID,
    glyph_key: GlyphKey,
    result: Option<RasterizedGlyph>,
}

struct CompiledNode {
    batches: Vec<RenderBatch>,
    commands: Vec<DrawCommand>,
    matrix_maps: HashMap<BatchId, HashMap<DrawListIndex, u8>>,
}

impl CompiledNode {
    fn new() -> CompiledNode {
        CompiledNode {
            batches: Vec::new(),
            commands: Vec::new(),
            matrix_maps: HashMap::new(),
        }
    }
}

struct DrawCommandBuilder {
    quad_program_id: ProgramId,
    glyph_program_id: ProgramId,
    device_pixel_ratio: f32,
    render_target_index: RenderTargetIndex,

    render_items: Vec<RenderItem>,
    vertex_buffer: VertexBuffer,
    // store shared clip buffers per CompiledNode
    // Since we have one CompiledNode per AABBTreeNode,
    // and AABBTreeNodes are processed in parallel,
    // this is the highest up these buffers can be stored
    clip_buffers: clipper::ClipBuffers,
}

impl DrawCommandBuilder {
    fn new(quad_program_id: ProgramId,
           glyph_program_id: ProgramId,
           device_pixel_ratio: f32,
           render_target_index: RenderTargetIndex) -> DrawCommandBuilder {
        DrawCommandBuilder {
            render_target_index: render_target_index,
            device_pixel_ratio: device_pixel_ratio,
            quad_program_id: quad_program_id,
            glyph_program_id: glyph_program_id,
            render_items: Vec::new(),
            vertex_buffer: VertexBuffer::new(),
            clip_buffers: clipper::ClipBuffers::new(),
        }
    }

    fn finalize(self) -> (Vec<RenderBatch>, Vec<DrawCommand>) {
        let mut current_batch: Option<RenderBatch> = None;
        let mut draw_commands = Vec::new();
        let mut batches = Vec::new();

        for item in self.render_items {
            match item.info {
                RenderItemInfo::Draw(ref info) => {
                    let program_id = match info.primitive {
                        Primitive::Rectangles |
                        Primitive::TriangleFan => {
                            self.quad_program_id
                        }
                        Primitive::Glyphs => {
                            self.glyph_program_id
                        }
                    };

                    let need_new_batch = current_batch.is_none() ||
                                         current_batch.as_ref().unwrap().can_add_to_batch(info,
                                                                                          &item.sort_key,
                                                                                          program_id) == false;

                    if need_new_batch {
                        if let Some(current_batch) = current_batch.take() {
                            draw_commands.push(DrawCommand {
                                render_target: self.render_target_index,
                                sort_key: current_batch.sort_key.clone(),
                                info: DrawCommandInfo::Batch(current_batch.batch_id),
                            });
                            batches.push(current_batch);
                        }
                        current_batch = Some(RenderBatch::new(BatchId::new(),
                                                              item.sort_key.clone(),
                                                              program_id,
                                                              info.color_texture_id,
                                                              info.mask_texture_id));
                    }

                    let batch = current_batch.as_mut().unwrap();
                    batch.add_draw_item(info,
                                        &self.vertex_buffer.vertices,
                                        &item.sort_key,
                                        self.device_pixel_ratio);
                }
                RenderItemInfo::Composite(ref info) => {
                    // When a composite is encountered - always flush any batches that are pending.
                    // TODO: It may be possible to be smarter about this in the future and avoid
                    // flushing the batches in some cases.
                    if let Some(current_batch) = current_batch.take() {
                        draw_commands.push(DrawCommand {
                            render_target: self.render_target_index,
                            sort_key: current_batch.sort_key.clone(),
                            info: DrawCommandInfo::Batch(current_batch.batch_id),
                        });
                        batches.push(current_batch);
                    }

                    let composite_info = CompositeInfo {
                        blend_mode: info.blend_mode,
                        rect: info.rect,
                        color_texture_id: info.color_texture_id,
                    };
                    let cmd = DrawCommand {
                        render_target: self.render_target_index,
                        sort_key: item.sort_key,
                        info: DrawCommandInfo::Composite(composite_info)
                    };
                    draw_commands.push(cmd);
                }
            }
        }

        if let Some(current_batch) = current_batch.take() {
            draw_commands.push(DrawCommand {
                render_target: self.render_target_index,
                sort_key: current_batch.sort_key.clone(),
                info: DrawCommandInfo::Batch(current_batch.batch_id),
            });
            batches.push(current_batch);
        }

        (batches, draw_commands)
    }
}

struct AABBTreeNode {
    rect: Rect<f32>,
    node_index: NodeIndex,

    // TODO: Use Option + NonZero here
    children: Option<NodeIndex>,

    is_visible: bool,

    src_items: Vec<DisplayItemKey>,

    resource_list: Option<ResourceList>,
    compiled_node: Option<CompiledNode>,
}

impl AABBTreeNode {
    fn new(rect: &Rect<f32>, node_index: NodeIndex) -> AABBTreeNode {
        AABBTreeNode {
            rect: rect.clone(),
            node_index: node_index,
            children: None,
            is_visible: false,
            resource_list: None,
            src_items: Vec::new(),
            compiled_node: None,
        }
    }

    #[inline]
    fn append_item(&mut self,
                   draw_list_index: usize,
                   item_index: usize) {
        let key = DisplayItemKey::new(draw_list_index, item_index);
        self.src_items.push(key);
    }

    fn compile(&mut self,
               flat_draw_lists: &FlatDrawListArray,
               white_image_info: &TextureCacheItem,
               mask_image_info: &TextureCacheItem,
               glyph_to_image_map: &HashMap<GlyphKey, ImageID, DefaultState<FnvHasher>>,
               raster_to_image_map: &HashMap<RasterItem, ImageID, DefaultState<FnvHasher>>,
               texture_cache: &TextureCache,
               node_rects: &Vec<Rect<f32>>,
               quad_program_id: ProgramId,
               glyph_program_id: ProgramId,
               device_pixel_ratio: f32) {
        let color_white = ColorF::new(1.0, 1.0, 1.0, 1.0);
        let mut compiled_node = CompiledNode::new();

        let mut draw_cmd_builders = HashMap::new();

        let iter = DisplayItemIterator::new(flat_draw_lists, &self.src_items);
        for key in iter {
            let (display_item, draw_context) = flat_draw_lists.get_item_and_draw_context(&key);

            if let Some(item_node_index) = display_item.node_index {
                if item_node_index == self.node_index {
                    let clip_rect = display_item.clip.main.intersection(&draw_context.overflow);

                    if let Some(clip_rect) = clip_rect {

                        let builder = match draw_cmd_builders.entry(draw_context.render_target_index) {
                            Vacant(entry) => {
                                entry.insert(DrawCommandBuilder::new(quad_program_id,
                                                                     glyph_program_id,
                                                                     device_pixel_ratio,
                                                                     draw_context.render_target_index))
                            }
                            Occupied(entry) => entry.into_mut(),
                        };

                        match display_item.item {
                            SpecificDisplayItem::Image(ref info) => {
                                let image = texture_cache.get(info.image_id);
                                builder.add_image(&key,
                                                        &display_item.rect,
                                                        &clip_rect,
                                                        &display_item.clip,
                                                        &info.stretch_size,
                                                        image,
                                                        mask_image_info,
                                                        raster_to_image_map,
                                                        &texture_cache,
                                                        &color_white);
                            }
                            SpecificDisplayItem::Text(ref info) => {
                                builder.add_text(&key,
                                                       draw_context,
                                                       info.font_id.clone(),
                                                       info.size,
                                                       info.blur_radius,
                                                       &info.color,
                                                       &info.glyphs,
                                                       mask_image_info,
                                                       &glyph_to_image_map,
                                                       &texture_cache);
                            }
                            SpecificDisplayItem::Rectangle(ref info) => {
                                builder.add_rectangle(&key,
                                                            &display_item.rect,
                                                            &clip_rect,
                                                            BoxShadowClipMode::Inset,
                                                            &display_item.clip,
                                                            white_image_info,
                                                            mask_image_info,
                                                            raster_to_image_map,
                                                            &texture_cache,
                                                            &info.color);
                            }
                            SpecificDisplayItem::Iframe(..) => {}
                            SpecificDisplayItem::Gradient(ref info) => {
                                builder.add_gradient(&key,
                                                           &display_item.rect,
                                                           &info.start_point,
                                                           &info.end_point,
                                                           &info.stops,
                                                           white_image_info,
                                                           mask_image_info);
                            }
                            SpecificDisplayItem::BoxShadow(ref info) => {
                                builder.add_box_shadow(&key,
                                                             &info.box_bounds,
                                                             &clip_rect,
                                                             &display_item.clip,
                                                             &info.offset,
                                                             &info.color,
                                                             info.blur_radius,
                                                             info.spread_radius,
                                                             info.border_radius,
                                                             info.clip_mode,
                                                             white_image_info,
                                                             mask_image_info,
                                                             raster_to_image_map,
                                                             texture_cache);
                            }
                            SpecificDisplayItem::Border(ref info) => {
                                builder.add_border(&key,
                                                     &display_item.rect,
                                                     info,
                                                     white_image_info,
                                                     mask_image_info,
                                                     raster_to_image_map,
                                                     texture_cache);
                            }
                            SpecificDisplayItem::Composite(ref info) => {
                                builder.add_composite(&key,
                                                            draw_context,
                                                            &display_item.rect,
                                                            info.texture_id,
                                                            info.blend_mode);
                            }
                        }
                    }
                } else {
                    // TODO: Cache this information!!!
                    let NodeIndex(node0) = item_node_index;
                    let NodeIndex(node1) = self.node_index;

                    let rect0 = &node_rects[node0 as usize];
                    let rect1 = &node_rects[node1 as usize];
                    let nodes_overlap = rect0.intersects(rect1);
                    if nodes_overlap {
                        if let Some(builder) = draw_cmd_builders.remove(&draw_context.render_target_index) {
                            let (batches, commands) = builder.finalize();
                            compiled_node.batches.extend(batches.into_iter());
                            compiled_node.commands.extend(commands.into_iter());
                        }
                    }
                }
            }
        }

        for (_, builder) in draw_cmd_builders.into_iter() {
            let (batches, commands) = builder.finalize();
            compiled_node.batches.extend(batches.into_iter());
            compiled_node.commands.extend(commands.into_iter());
        }

        self.compiled_node = Some(compiled_node);
    }
}

struct AABBTree {
    nodes: Vec<AABBTreeNode>,
    split_size: f32,
}

impl AABBTree {
    fn new(split_size: f32) -> AABBTree {
        AABBTree {
            nodes: Vec::new(),
            split_size: split_size,
        }
    }

    fn init(&mut self, scene_rect: &Rect<f32>) {
        self.nodes.clear();

        let root_node = AABBTreeNode::new(scene_rect, NodeIndex(0));
        self.nodes.push(root_node);
    }

    #[allow(dead_code)]
    fn print(&self, node_index: NodeIndex, level: u32) {
        let mut indent = String::new();
        for _ in 0..level {
            indent.push_str("  ");
        }

        let node = self.node(node_index);
        println!("{}n={:?} r={:?} c={:?}", indent, node_index, node.rect, node.children);

        if let Some(child_index) = node.children {
            let NodeIndex(child_index) = child_index;
            self.print(NodeIndex(child_index+0), level+1);
            self.print(NodeIndex(child_index+1), level+1);
        }
    }

    #[inline(always)]
    fn node(&self, index: NodeIndex) -> &AABBTreeNode {
        let NodeIndex(index) = index;
        &self.nodes[index as usize]
    }

    #[inline(always)]
    fn node_mut(&mut self, index: NodeIndex) -> &mut AABBTreeNode {
        let NodeIndex(index) = index;
        &mut self.nodes[index as usize]
    }

    // TODO: temp hack to test if this idea works
    fn node_rects(&self) -> Vec<Rect<f32>> {
        let mut rects = Vec::new();
        for node in &self.nodes {
            rects.push(node.rect);
        }
        rects
    }

    #[inline]
    fn find_best_node(&mut self,
                      node_index: NodeIndex,
                      rect: &Rect<f32>) -> Option<NodeIndex> {
        self.split_if_needed(node_index);

        if let Some(child_node_index) = self.node(node_index).children {
            let NodeIndex(child_node_index) = child_node_index;
            let left_node_index = NodeIndex(child_node_index + 0);
            let right_node_index = NodeIndex(child_node_index + 1);

            let left_intersect = self.node(left_node_index).rect.intersects(rect);
            let right_intersect = self.node(right_node_index).rect.intersects(rect);

            if left_intersect && right_intersect {
                Some(node_index)
            } else if left_intersect {
                self.find_best_node(left_node_index, rect)
            } else if right_intersect {
                self.find_best_node(right_node_index, rect)
            } else {
                None
            }
        } else {
            Some(node_index)
        }
    }

    #[inline]
    fn insert(&mut self,
              rect: &Rect<f32>,
              draw_list_index: usize,
              item_index: usize) -> Option<NodeIndex> {
        let node_index = self.find_best_node(NodeIndex(0), rect);
        if let Some(node_index) = node_index {
            let node = self.node_mut(node_index);
            node.append_item(draw_list_index, item_index);
        }
        node_index
    }

    fn split_if_needed(&mut self, node_index: NodeIndex) {
        if self.node(node_index).children.is_none() {
            let rect = self.node(node_index).rect.clone();

            let child_rects = if rect.size.width > self.split_size &&
                                 rect.size.width > rect.size.height {
                let new_width = rect.size.width * 0.5;

                let left = Rect::new(rect.origin, Size2D::new(new_width, rect.size.height));
                let right = Rect::new(rect.origin + Point2D::new(new_width, 0.0),
                                      Size2D::new(rect.size.width - new_width, rect.size.height));

                Some((left, right))
            } else if rect.size.height > self.split_size {
                let new_height = rect.size.height * 0.5;

                let left = Rect::new(rect.origin, Size2D::new(rect.size.width, new_height));
                let right = Rect::new(rect.origin + Point2D::new(0.0, new_height),
                                      Size2D::new(rect.size.width, rect.size.height - new_height));

                Some((left, right))
            } else {
                None
            };

            if let Some((left_rect, right_rect)) = child_rects {
                let child_node_index = self.nodes.len() as u32;

                let left_node = AABBTreeNode::new(&left_rect, NodeIndex(child_node_index+0));
                self.nodes.push(left_node);

                let right_node = AABBTreeNode::new(&right_rect, NodeIndex(child_node_index+1));
                self.nodes.push(right_node);

                self.node_mut(node_index).children = Some(NodeIndex(child_node_index));
            }
        }
    }

    fn check_node_visibility(&mut self,
                             node_index: NodeIndex,
                             rect: &Rect<f32>) {
        let children = {
            let node = self.node_mut(node_index);
            if node.rect.intersects(rect) {
                node.is_visible = true;
                node.children
            } else {
                return;
            }
        };

        if let Some(child_index) = children {
            let NodeIndex(child_index) = child_index;
            self.check_node_visibility(NodeIndex(child_index+0), rect);
            self.check_node_visibility(NodeIndex(child_index+1), rect);
        }
    }

    fn cull(&mut self, rect: &Rect<f32>) {
        let _pf = util::ProfileScope::new("  cull");
        for node in &mut self.nodes {
            node.is_visible = false;
        }
        if self.nodes.len() > 0 {
            self.check_node_visibility(NodeIndex(0), &rect);
        }
    }
}

#[derive(Debug)]
struct IframeInfo {
    offset: Point2D<f32>,
    id: PipelineId,
}

impl IframeInfo {
    fn new(id: PipelineId, offset: Point2D<f32>) -> IframeInfo {
        IframeInfo {
            offset: offset,
            id: id,
        }
    }
}

struct RootStackingContext {
    pipeline_id: PipelineId,
    epoch: Epoch,
    background_color: ColorF,
    stacking_context: StackingContext,
}

enum StackingContextKind<'a> {
    Normal(&'a StackingContext),
    Root(&'a RootStackingContext)
}

pub struct RenderBackend {
    api_rx: Receiver<ApiMsg>,
    result_tx: Sender<ResultMsg>,
    viewport: Rect<i32>,
    device_pixel_ratio: f32,

    quad_program_id: ProgramId,
    glyph_program_id: ProgramId,
    white_image_id: ImageID,
    dummy_mask_image_id: ImageID,

    texture_cache: TextureCache,
    font_templates: HashMap<Atom, FontTemplate, DefaultState<FnvHasher>>,
    image_templates: HashMap<ImageID, ImageResource, DefaultState<FnvHasher>>,
    glyph_to_image_map: HashMap<GlyphKey, ImageID, DefaultState<FnvHasher>>,
    raster_to_image_map: HashMap<RasterItem, ImageID, DefaultState<FnvHasher>>,

    display_list_map: DisplayListMap,
    draw_list_map: DrawListMap,
    stacking_contexts: StackingContextMap,

    scene: Scene,
}

impl RenderBackend {
    pub fn new(rx: Receiver<ApiMsg>,
               tx: Sender<ResultMsg>,
               viewport: Rect<i32>,
               device_pixel_ratio: f32,
               quad_program_id: ProgramId,
               glyph_program_id: ProgramId,
               white_image_id: ImageID,
               dummy_mask_image_id: ImageID,
               texture_cache: TextureCache,
               /*vao_ids: Vec<VAOId>*/) -> RenderBackend {
        let mut backend = RenderBackend {
            api_rx: rx,
            result_tx: tx,
            viewport: viewport,
            device_pixel_ratio: device_pixel_ratio,

            quad_program_id: quad_program_id,
            glyph_program_id: glyph_program_id,
            white_image_id: white_image_id,
            dummy_mask_image_id: dummy_mask_image_id,
            texture_cache: texture_cache,
            //vao_ids: vao_ids,

            font_templates: HashMap::with_hash_state(Default::default()),
            image_templates: HashMap::with_hash_state(Default::default()),
            glyph_to_image_map: HashMap::with_hash_state(Default::default()),
            raster_to_image_map: HashMap::with_hash_state(Default::default()),

            scene: Scene::new(),
            display_list_map: HashMap::with_hash_state(Default::default()),
            draw_list_map: HashMap::with_hash_state(Default::default()),
            stacking_contexts: HashMap::with_hash_state(Default::default()),
        };

        let thread_count = backend.scene.thread_pool.thread_count() as usize;
        backend.scene.thread_pool.scoped(|scope| {
            for _ in 0..thread_count {
                scope.execute(|| {
                    FONT_CONTEXT.with(|_| {
                        FONT_CONTEXT_COUNT.fetch_add(1, SeqCst);
                        while FONT_CONTEXT_COUNT.load(SeqCst) != thread_count {
                            thread::sleep_ms(1);
                        }
                    });
                });
            }
        });

        backend
    }

    fn remove_draw_list(&mut self, draw_list_id: Option<DrawListID>) {
        if let Some(id) = draw_list_id {
            self.draw_list_map.remove(&id).unwrap();
        }
    }

    fn add_draw_list(&mut self, draw_list: DrawList) -> Option<DrawListID> {
        if draw_list.item_count() > 0 {
            let id = DrawListID::new();
            self.draw_list_map.insert(id, draw_list);
            Some(id)
        } else {
            None
        }
    }

    pub fn run(&mut self, notifier: Box<RenderNotifier>) {
        let mut notifier = notifier;

        loop {
            let msg = self.api_rx.recv();

            match msg {
                Ok(msg) => {
                    match msg {
                        ApiMsg::AddFont(id, bytes) => {
                            self.font_templates.insert(id, FontTemplate {
                                bytes: Arc::new(bytes),
                            });
                        }
                        ApiMsg::AddImage(id, width, height, format, bytes) => {
                            let image = ImageResource {
                                bytes: bytes,
                                width: width,
                                height: height,
                                format: format,
                            };
                            self.image_templates.insert(id, image);
                        }
                        ApiMsg::AddDisplayList(id, pipeline_id, epoch, display_list_builder) => {
                            let display_list = DisplayList {
                                mode: display_list_builder.mode,
                                pipeline_id: pipeline_id,
                                epoch: epoch,
                                background_and_borders_id: self.add_draw_list(display_list_builder.background_and_borders),
                                block_backgrounds_and_borders_id: self.add_draw_list(display_list_builder.block_backgrounds_and_borders),
                                floats_id: self.add_draw_list(display_list_builder.floats),
                                content_id: self.add_draw_list(display_list_builder.content),
                                positioned_content_id: self.add_draw_list(display_list_builder.positioned_content),
                                outlines_id: self.add_draw_list(display_list_builder.outlines),
                            };

                            self.display_list_map.insert(id, display_list);
                        }
                        ApiMsg::SetRootStackingContext(stacking_context, background_color, epoch, pipeline_id) => {
                            let _pf = util::ProfileScope::new("SetRootStackingContext");

                            // Return all current draw lists to the hash
                            for flat_draw_list in self.scene.flat_draw_lists.drain(..) {
                                if let Some(id) = flat_draw_list.id {
                                    self.draw_list_map.insert(id, flat_draw_list.draw_list);
                                }
                            }

                            // Remove any old draw lists and display lists for this pipeline
                            let old_display_list_keys: Vec<_> = self.display_list_map.iter()
                                                                    .filter(|&(_, ref v)| {
                                                                        v.pipeline_id == pipeline_id &&
                                                                        v.epoch < epoch
                                                                    })
                                                                    .map(|(k, _)| k.clone())
                                                                    .collect();

                            for key in &old_display_list_keys {
                                let display_list = self.display_list_map.remove(key).unwrap();
                                self.remove_draw_list(display_list.background_and_borders_id);
                                self.remove_draw_list(display_list.block_backgrounds_and_borders_id);
                                self.remove_draw_list(display_list.floats_id);
                                self.remove_draw_list(display_list.content_id);
                                self.remove_draw_list(display_list.positioned_content_id);
                                self.remove_draw_list(display_list.outlines_id);
                            }

                            self.stacking_contexts.insert(pipeline_id, RootStackingContext {
                                pipeline_id: pipeline_id,
                                epoch: epoch,
                                background_color: background_color,
                                stacking_context: stacking_context,
                            });

                            self.build_scene();
                            self.render(&mut *notifier);
                        }
                        ApiMsg::Scroll(delta) => {
                            let _pf = util::ProfileScope::new("Scroll");

                            self.scroll(delta);
                            self.render(&mut *notifier);
                        }
                    }
                }
                Err(..) => {
                    break;
                }
            }
        }
    }

    fn build_scene(&mut self) {
        // Flatten the stacking context hierarchy
        // TODO: Fixme!
        let root_pipeline_id = PipelineId(0, 0);
        if let Some(root_sc) = self.stacking_contexts.get(&root_pipeline_id) {
            // Clear out any state and return draw lists (if needed)
            self.scene.reset(&mut self.texture_cache);

            let size = Size2D::new(self.viewport.size.width as u32,
                                   self.viewport.size.height as u32);
            self.scene.push_render_target(size, None);
            self.scene.flatten_stacking_context(StackingContextKind::Root(root_sc),
                                                &Matrix4::identity(),
                                                &self.display_list_map,
                                                &mut self.draw_list_map,
                                                &self.stacking_contexts,
                                                self.device_pixel_ratio,
                                                &mut self.texture_cache);
            self.scene.pop_render_target();

            // Init the AABB culling tree(s)
            self.scene.build_aabb_tree(&root_sc.stacking_context.overflow);
        }
    }

    fn render(&mut self, notifier: &mut RenderNotifier) {
        let frame = self.scene.build_frame(&self.viewport,
                                           self.device_pixel_ratio,
                                           &mut self.raster_to_image_map,
                                           &mut self.glyph_to_image_map,
                                           &self.image_templates,
                                           &self.font_templates,
                                           &mut self.texture_cache,
                                           self.white_image_id,
                                           self.dummy_mask_image_id,
                                           self.quad_program_id,
                                           self.glyph_program_id);

        let pending_update = self.texture_cache.pending_updates();
        if pending_update.updates.len() > 0 {
            self.result_tx.send(ResultMsg::UpdateTextureCache(pending_update)).unwrap();
        }

        let pending_update = self.scene.pending_updates();
        if pending_update.updates.len() > 0 {
            self.result_tx.send(ResultMsg::UpdateBatches(pending_update)).unwrap();
        }

        self.result_tx.send(ResultMsg::NewFrame(frame)).unwrap();
        notifier.new_frame_ready();
    }

    fn scroll(&mut self, delta: Point2D<f32>) {
        self.scene.scroll(delta);
    }

}

#[derive(Debug)]
enum Primitive {
    Rectangles,     // 4 vertices per rect
    TriangleFan,    // simple triangle fan (typically from clipper)
    Glyphs,         // font glyphs (some platforms may specialize shader)
}

#[derive(Debug)]
struct DrawRenderItem {
    color_texture_id: TextureId,
    mask_texture_id: TextureId,
    first_vertex: u32,
    vertex_count: u32,
    primitive: Primitive,
}

#[derive(Debug)]
struct CompositeRenderItem {
    blend_mode: MixBlendMode,
    rect: Rect<u32>,
    color_texture_id: TextureId,
}

#[derive(Debug)]
enum RenderItemInfo {
    Draw(DrawRenderItem),
    Composite(CompositeRenderItem),
}

#[derive(Debug)]
struct RenderItem {
    sort_key: DisplayItemKey,       // TODO: Make this smaller!
    info: RenderItemInfo,
}

struct VertexBuffer {
    vertices: Vec<WorkVertex>,
}

impl VertexBuffer {
    fn new() -> VertexBuffer {
        VertexBuffer {
            vertices: Vec::new(),
        }
    }

    fn len(&self) -> u32 {
        self.vertices.len() as u32
    }

    #[inline]
    fn push(&mut self,
            x: f32,
            y: f32,
            color: &ColorF,
            s: f32,
            t: f32) {
        self.vertices.push(WorkVertex::new(x,
                                           y,
                                           color,
                                           s,
                                           t,
                                           0.0,
                                           0.0));
    }

    #[inline]
    fn push_white(&mut self,
                  x: f32,
                  y: f32,
                  color: &ColorF) {
        self.vertices.push(WorkVertex::new(x,
                                           y,
                                           color,
                                           0.0,
                                           0.0,
                                           0.0,
                                           0.0));
    }

    #[inline]
    fn push_masked(&mut self,
                   x: f32,
                   y: f32,
                   color: &ColorF,
                   ms: f32,
                   mt: f32) {
        self.vertices.push(WorkVertex::new(x,
                                           y,
                                           color,
                                           0.0,
                                           0.0,
                                           ms,
                                           mt));
    }

    #[inline]
    fn push_textured_and_masked(&mut self,
                                x: f32,
                                y: f32,
                                color: &ColorF,
                                s: f32,
                                t: f32,
                                ms: f32,
                                mt: f32) {
        self.vertices.push(WorkVertex::new(x,
                                           y,
                                           color,
                                           s,
                                           t,
                                           ms,
                                           mt));
    }

    #[inline]
    fn push_vertex(&mut self, v: WorkVertex) {
        self.vertices.push(v);
    }

    #[inline]
    fn extend(&mut self, vb: VertexBuffer) {
        self.vertices.extend(vb.vertices);
    }
}

impl DrawCommandBuilder {
    fn add_rectangle(&mut self,
                     sort_key: &DisplayItemKey,
                     rect: &Rect<f32>,
                     clip: &Rect<f32>,
                     clip_mode: BoxShadowClipMode,
                     clip_region: &ClipRegion,
                     image_info: &TextureCacheItem,
                     dummy_mask_image: &TextureCacheItem,
                     raster_to_image_map: &HashMap<RasterItem, ImageID, DefaultState<FnvHasher>>,
                     texture_cache: &TextureCache,
                     color: &ColorF) {
        self.add_axis_aligned_gradient(sort_key,
                                       rect,
                                       clip,
                                       clip_mode,
                                       clip_region,
                                       image_info,
                                       dummy_mask_image,
                                       raster_to_image_map,
                                       texture_cache,
                                       &[*color, *color, *color, *color])
    }

    fn add_composite(&mut self,
                     sort_key: &DisplayItemKey,
                     draw_context: &DrawContext,
                     rect: &Rect<f32>,
                     texture_id: RenderTargetID,
                     blend_mode: MixBlendMode) {
        let RenderTargetID(texture_id) = texture_id;

        let origin = draw_context.final_transform.transform_point(&rect.origin);
        let origin = Point2D::new(origin.x as u32, origin.y as u32);
        let size = Size2D::new(rect.size.width as u32, rect.size.height as u32);

        let render_item = RenderItem {
            sort_key: sort_key.clone(),
            info: RenderItemInfo::Composite(CompositeRenderItem {
                blend_mode: blend_mode,
                rect: Rect::new(origin, size),
                color_texture_id: TextureId(texture_id),
            }),
        };

        self.render_items.push(render_item);
    }

    fn add_image(&mut self,
                 sort_key: &DisplayItemKey,
                 rect: &Rect<f32>,
                 clip_rect: &Rect<f32>,
                 clip_region: &ClipRegion,
                 stretch_size: &Size2D<f32>,
                 image_info: &TextureCacheItem,
                 dummy_mask_image: &TextureCacheItem,
                 raster_to_image_map: &HashMap<RasterItem, ImageID, DefaultState<FnvHasher>>,
                 texture_cache: &TextureCache,
                 color: &ColorF) {
        debug_assert!(stretch_size.width > 0.0 && stretch_size.height > 0.0);       // Should be caught higher up

        let uv_origin = Point2D::new(image_info.u0, image_info.v0);
        let uv_size = Size2D::new(image_info.u1 - image_info.u0,
                                  image_info.v1 - image_info.v0);
        let uv = Rect::new(uv_origin, uv_size);

        if rect.size.width == stretch_size.width && rect.size.height == stretch_size.height {
            push_rect(&mut self.render_items,
                      &mut self.vertex_buffer,
                      color,
                      image_info,
                      dummy_mask_image,
                      clip_rect,
                      clip_region,
                      &sort_key,
                      raster_to_image_map,
                      texture_cache,
                      rect,
                      &uv);
        } else {
            let mut y_offset = 0.0;
            while y_offset < rect.size.height {
                let mut x_offset = 0.0;
                while x_offset < rect.size.width {

                    let origin = Point2D::new(rect.origin.x + x_offset, rect.origin.y + y_offset);
                    let tiled_rect = Rect::new(origin, stretch_size.clone());

                    push_rect(&mut self.render_items,
                              &mut self.vertex_buffer,
                              color,
                              image_info,
                              dummy_mask_image,
                              clip_rect,
                              clip_region,
                              &sort_key,
                              raster_to_image_map,
                              texture_cache,
                              &tiled_rect,
                              &uv);

                    x_offset = x_offset + stretch_size.width;
                }

                y_offset = y_offset + stretch_size.height;
            }
        }

        fn push_rect(render_items: &mut Vec<RenderItem>,
                     vertex_buffer: &mut VertexBuffer,
                     color: &ColorF,
                     image_info: &TextureCacheItem,
                     dummy_mask_image: &TextureCacheItem,
                     clip_rect: &Rect<f32>,
                     clip_region: &ClipRegion,
                     sort_key: &DisplayItemKey,
                     raster_to_image_map: &HashMap<RasterItem, ImageID, DefaultState<FnvHasher>>,
                     texture_cache: &TextureCache,
                     rect: &Rect<f32>,
                     uv: &Rect<f32>) {
            for clip_region in clipper::clip_rect_with_mode_and_to_region_pos_uv(
                    rect,
                    &uv,
                    clip_rect,
                    BoxShadowClipMode::Inset,
                    clip_region) {
                let rect = clip_region.rect_result.rect();
                let uv = clip_region.rect_result.uv_rect();
                let mask = match clip_region.mask_result {
                    None => dummy_mask_image,
                    Some(ref mask_result) => {
                        mask_for_border_radius(dummy_mask_image,
                                               raster_to_image_map,
                                               texture_cache,
                                               mask_result.border_radius)
                    }
                };

                let first_vertex = vertex_buffer.len();
                let muv = clip_region.muv(&mask);
                vertex_buffer.push_textured_and_masked(rect.origin.x, rect.origin.y,
                                                       color,
                                                       uv.origin.x, uv.origin.y,
                                                       muv.origin.x, muv.origin.y);
                vertex_buffer.push_textured_and_masked(rect.max_x(), rect.origin.y,
                                                       color,
                                                       uv.max_x(), uv.origin.y,
                                                       muv.max_x(), muv.origin.y);
                vertex_buffer.push_textured_and_masked(rect.origin.x, rect.max_y(),
                                                       color,
                                                       uv.origin.x, uv.max_y(),
                                                       muv.origin.x, muv.max_y());
                vertex_buffer.push_textured_and_masked(rect.max_x(), rect.max_y(),
                                                       color,
                                                       uv.max_x(), uv.max_y(),
                                                       muv.max_x(), muv.max_y());

                let render_item = RenderItem {
                    sort_key: (*sort_key).clone(),
                    info: RenderItemInfo::Draw(DrawRenderItem {
                        color_texture_id: image_info.texture_id,
                        mask_texture_id: mask.texture_id,
                        primitive: Primitive::Rectangles,
                        first_vertex: first_vertex,
                        vertex_count: 4,
                    }),
                };
                render_items.push(render_item);
            }
        }
    }

    fn add_text(&mut self,
                sort_key: &DisplayItemKey,
                draw_context: &DrawContext,
                font_id: Atom,
                size: Au,
                blur_radius: Au,
                color: &ColorF,
                glyphs: &Vec<GlyphInstance>,
                dummy_mask_image: &TextureCacheItem,
                glyph_to_image_map: &HashMap<GlyphKey, ImageID, DefaultState<FnvHasher>>,
                texture_cache: &TextureCache) {
        // Logic below to pick the primary render item depends on len > 0!
        assert!(glyphs.len() > 0);

        let device_pixel_ratio = draw_context.device_pixel_ratio;

        let mut glyph_key = GlyphKey::new(font_id, size, blur_radius, glyphs[0].index);

        let blur_offset = blur_radius.to_f32_px() * (BLUR_INFLATION_FACTOR as f32) / 2.0;

        let first_image_id = glyph_to_image_map.get(&glyph_key).unwrap();
        let first_image_info = texture_cache.get(*first_image_id);

        let mut primary_render_item = DrawRenderItem {
            color_texture_id: first_image_info.texture_id,
            mask_texture_id: dummy_mask_image.texture_id,
            primitive: Primitive::Glyphs,
            first_vertex: self.vertex_buffer.len(),
            vertex_count: 0,
        };

        let mut other_render_items: HashMap<TextureId, VertexBuffer> = HashMap::new();

        for glyph in glyphs {
            glyph_key.index = glyph.index;
            let image_id = glyph_to_image_map.get(&glyph_key).unwrap();
            let image_info = texture_cache.get(*image_id);

            if image_info.width > 0 && image_info.height > 0 {
                let x0 = glyph.x + image_info.x0 as f32 / device_pixel_ratio - blur_offset;
                let y0 = glyph.y - image_info.y0 as f32 / device_pixel_ratio - blur_offset;

                let x1 = x0 + image_info.width as f32 / device_pixel_ratio;
                let y1 = y0 + image_info.height as f32 / device_pixel_ratio;

                if image_info.texture_id == first_image_info.texture_id {
                    self.vertex_buffer.push(x0, y0, color, image_info.u0, image_info.v0);
                    self.vertex_buffer.push(x1, y0, color, image_info.u1, image_info.v0);
                    self.vertex_buffer.push(x0, y1, color, image_info.u0, image_info.v1);
                    self.vertex_buffer.push(x1, y1, color, image_info.u1, image_info.v1);
                    primary_render_item.vertex_count += 4;
                } else {
                    let vertex_buffer = match other_render_items.entry(image_info.texture_id) {
                        Occupied(entry) => {
                            entry.into_mut()
                        }
                        Vacant(entry) => {
                            entry.insert(VertexBuffer::new())
                        }
                    };
                    vertex_buffer.push(x0, y0, color, image_info.u0, image_info.v0);
                    vertex_buffer.push(x1, y0, color, image_info.u1, image_info.v0);
                    vertex_buffer.push(x0, y1, color, image_info.u0, image_info.v1);
                    vertex_buffer.push(x1, y1, color, image_info.u1, image_info.v1);
                }
            }
        }

        if primary_render_item.vertex_count > 0 {
            self.render_items.push(RenderItem {
                sort_key: sort_key.clone(),
                info: RenderItemInfo::Draw(primary_render_item),
            });
        }

        for (texture_id, vertex_buffer) in other_render_items {
            let render_item = RenderItem {
                sort_key: sort_key.clone(),
                info: RenderItemInfo::Draw(DrawRenderItem {
                    color_texture_id: texture_id,
                    mask_texture_id: dummy_mask_image.texture_id,
                    primitive: Primitive::Glyphs,
                    first_vertex: self.vertex_buffer.len(),
                    vertex_count: vertex_buffer.len() as u32,
                }),
            };
            self.vertex_buffer.extend(vertex_buffer);
            self.render_items.push(render_item);
        }
    }

    // Colors are in the order: top left, top right, bottom right, bottom left.
    fn add_axis_aligned_gradient(&mut self,
                                 sort_key: &DisplayItemKey,
                                 rect: &Rect<f32>,
                                 clip: &Rect<f32>,
                                 clip_mode: BoxShadowClipMode,
                                 clip_region: &ClipRegion,
                                 image_info: &TextureCacheItem,
                                 dummy_mask_image: &TextureCacheItem,
                                 raster_to_image_map: &HashMap<RasterItem,
                                                               ImageID,
                                                               DefaultState<FnvHasher>>,
                                 texture_cache: &TextureCache,
                                 colors: &[ColorF; 4]) {
        if rect.size.width == 0.0 || rect.size.height == 0.0 {
            return
        }

        let uv_origin = Point2D::new(image_info.u0, image_info.v0);
        let uv_size = Size2D::new(image_info.u1 - image_info.u0, image_info.v1 - image_info.v0);
        let uv = Rect::new(uv_origin, uv_size);

        // TODO(pcwalton): Clip colors too!
        for clip_region in clipper::clip_rect_with_mode_and_to_region_pos_uv(rect,
                                                                             &uv,
                                                                             clip,
                                                                             clip_mode,
                                                                             clip_region) {
            let mask = match clip_region.mask_result {
                None => dummy_mask_image,
                Some(ref mask_result) => {
                    mask_for_border_radius(dummy_mask_image,
                                           raster_to_image_map,
                                           texture_cache,
                                           mask_result.border_radius)
                }
            };

            let render_item = RenderItem {
                sort_key: sort_key.clone(),
                info: RenderItemInfo::Draw(DrawRenderItem {
                    color_texture_id: image_info.texture_id,
                    mask_texture_id: mask.texture_id,
                    primitive: Primitive::Rectangles,
                    vertex_count: 4,
                    first_vertex: self.vertex_buffer.len(),
                }),
            };

            let muv = clip_region.muv(&mask);
            self.vertex_buffer.push_textured_and_masked(clip_region.rect_result.x0,
                                                        clip_region.rect_result.y0,
                                                        &colors[0],
                                                        clip_region.rect_result.u0,
                                                        clip_region.rect_result.v0,
                                                        muv.origin.x, muv.origin.y);
            self.vertex_buffer.push_textured_and_masked(clip_region.rect_result.x1,
                                                        clip_region.rect_result.y0,
                                                        &colors[1],
                                                        clip_region.rect_result.u1,
                                                        clip_region.rect_result.v0,
                                                        muv.max_x(), muv.origin.y);
            self.vertex_buffer.push_textured_and_masked(clip_region.rect_result.x0,
                                                        clip_region.rect_result.y1,
                                                        &colors[3],
                                                        clip_region.rect_result.u0,
                                                        clip_region.rect_result.v1,
                                                        muv.origin.x, muv.max_y());
            self.vertex_buffer.push_textured_and_masked(clip_region.rect_result.x1,
                                                        clip_region.rect_result.y1,
                                                        &colors[2],
                                                        clip_region.rect_result.u1,
                                                        clip_region.rect_result.v1,
                                                        muv.max_x(), muv.max_y());

            self.render_items.push(render_item);
        }
    }

    fn add_gradient(&mut self,
                    sort_key: &DisplayItemKey,
                    rect: &Rect<f32>,
                    start_point: &Point2D<f32>,
                    end_point: &Point2D<f32>,
                    stops: &[GradientStop],
                    image: &TextureCacheItem,
                    dummy_mask_image: &TextureCacheItem) {
        debug_assert!(stops.len() >= 2);

        let x0 = rect.origin.x;
        let x1 = x0 + rect.size.width;
        let y0 = rect.origin.y;
        let y1 = y0 + rect.size.height;

        let clip_polygon = [
            Point2D::new(x0, y0),
            Point2D::new(x1, y0),
            Point2D::new(x1, y1),
            Point2D::new(x0, y1),
        ];

        let dir_x = end_point.x - start_point.x;
        let dir_y = end_point.y - start_point.y;
        let dir_len = (dir_x * dir_x + dir_y * dir_y).sqrt();
        let dir_xn = dir_x / dir_len;
        let dir_yn = dir_y / dir_len;
        let perp_xn = -dir_yn;
        let perp_yn = dir_xn;

        for i in 0..stops.len()-1 {
            let stop0 = &stops[i];
            let stop1 = &stops[i+1];

            let color0 = &stop0.color;
            let color1 = &stop1.color;

            let start_x = start_point.x + stop0.offset * (end_point.x - start_point.x);
            let start_y = start_point.y + stop0.offset * (end_point.y - start_point.y);

            let end_x = start_point.x + stop1.offset * (end_point.x - start_point.x);
            let end_y = start_point.y + stop1.offset * (end_point.y - start_point.y);

            let len_scale = 1000.0;     // todo: determine this properly!!

            let x0 = start_x - perp_xn * len_scale;
            let y0 = start_y - perp_yn * len_scale;

            let x1 = end_x - perp_xn * len_scale;
            let y1 = end_y - perp_yn * len_scale;

            let x2 = end_x + perp_xn * len_scale;
            let y2 = end_y + perp_yn * len_scale;

            let x3 = start_x + perp_xn * len_scale;
            let y3 = start_y + perp_yn * len_scale;

            let gradient_polygon = [
                WorkVertex::new(x0, y0, color0, 0.0, 0.0, 0.0, 0.0),
                WorkVertex::new(x1, y1, color1, 0.0, 0.0, 0.0, 0.0),
                WorkVertex::new(x2, y2, color1, 0.0, 0.0, 0.0, 0.0),
                WorkVertex::new(x3, y3, color0, 0.0, 0.0, 0.0, 0.0),
            ];

            { // scope for  buffers
                let buffers = &mut self.clip_buffers;
                let clip_result = clipper::clip_polygon(buffers, &gradient_polygon, &clip_polygon);

                if clip_result.len() >= 3 {
                    let render_item = RenderItem {
                        sort_key: sort_key.clone(),
                        info: RenderItemInfo::Draw(DrawRenderItem {
                            color_texture_id: image.texture_id,
                            mask_texture_id: dummy_mask_image.texture_id,
                            primitive: Primitive::TriangleFan,
                            first_vertex: self.vertex_buffer.len(),
                            vertex_count: clip_result.len() as u32,
                        }),
                    };

                    for vert in clip_result {
                        self.vertex_buffer.push_vertex(vert.clone());
                    }

                    self.render_items.push(render_item);
                }
            }
        }
    }

    fn add_box_shadow(&mut self,
                      sort_key: &DisplayItemKey,
                      box_bounds: &Rect<f32>,
                      clip: &Rect<f32>,
                      clip_region: &ClipRegion,
                      box_offset: &Point2D<f32>,
                      color: &ColorF,
                      blur_radius: f32,
                      spread_radius: f32,
                      border_radius: f32,
                      clip_mode: BoxShadowClipMode,
                      white_image: &TextureCacheItem,
                      dummy_mask_image: &TextureCacheItem,
                      raster_to_image_map: &HashMap<RasterItem, ImageID, DefaultState<FnvHasher>>,
                      texture_cache: &TextureCache) {
        let mut rect = box_bounds.clone();
        rect.origin.x += box_offset.x;
        rect.origin.y += box_offset.y;

        // Fast path.
        if blur_radius == 0.0 && spread_radius == 0.0 && clip_mode == BoxShadowClipMode::None {
            self.add_rectangle(sort_key,
                               &rect,
                               clip,
                               BoxShadowClipMode::Inset,
                               clip_region,
                               white_image,
                               dummy_mask_image,
                               raster_to_image_map,
                               texture_cache,
                               color);
            return;
        }

        // Draw the corners.
        //
        //      +--+------------------+--+
        //      |##|                  |##|
        //      +--+------------------+--+
        //      |  |                  |  |
        //      |  |                  |  |
        //      |  |                  |  |
        //      +--+------------------+--+
        //      |##|                  |##|
        //      +--+------------------+--+

        let side_radius = border_radius + blur_radius;
        let tl_outer = rect.origin;
        let tl_inner = tl_outer + Point2D::new(side_radius, side_radius);
        let tr_outer = rect.top_right();
        let tr_inner = tr_outer + Point2D::new(-side_radius, side_radius);
        let bl_outer = rect.bottom_left();
        let bl_inner = bl_outer + Point2D::new(side_radius, -side_radius);
        let br_outer = rect.bottom_right();
        let br_inner = br_outer + Point2D::new(-side_radius, -side_radius);

        self.add_box_shadow_corner(sort_key,
                                   &tl_outer,
                                   &tl_inner,
                                   box_bounds,
                                   &color,
                                   blur_radius,
                                   border_radius,
                                   clip_mode,
                                   white_image,
                                   dummy_mask_image,
                                   raster_to_image_map,
                                   texture_cache);
        self.add_box_shadow_corner(sort_key,
                                   &tr_outer,
                                   &tr_inner,
                                   box_bounds,
                                   &color,
                                   blur_radius,
                                   border_radius,
                                   clip_mode,
                                   white_image,
                                   dummy_mask_image,
                                   raster_to_image_map,
                                   texture_cache);
        self.add_box_shadow_corner(sort_key,
                                   &bl_outer,
                                   &bl_inner,
                                   box_bounds,
                                   &color,
                                   blur_radius,
                                   border_radius,
                                   clip_mode,
                                   white_image,
                                   dummy_mask_image,
                                   raster_to_image_map,
                                   texture_cache);
        self.add_box_shadow_corner(sort_key,
                                   &br_outer,
                                   &br_inner,
                                   box_bounds,
                                   &color,
                                   blur_radius,
                                   border_radius,
                                   clip_mode,
                                   white_image,
                                   dummy_mask_image,
                                   raster_to_image_map,
                                   texture_cache);

        // Draw the sides.
        //
        //      +--+------------------+--+
        //      |  |##################|  |
        //      +--+------------------+--+
        //      |##|                  |##|
        //      |##|                  |##|
        //      |##|                  |##|
        //      +--+------------------+--+
        //      |  |##################|  |
        //      +--+------------------+--+

        let transparent = ColorF {
            a: 0.0,
            ..*color
        };
        let blur_diameter = blur_radius + blur_radius;
        let twice_blur_diameter = blur_diameter + blur_diameter;
        let twice_side_radius = side_radius + side_radius;
        let horizontal_size = Size2D::new(rect.size.width - twice_side_radius, blur_diameter);
        let vertical_size = Size2D::new(blur_diameter, rect.size.height - twice_side_radius);
        let top_rect = Rect::new(tl_outer + Point2D::new(side_radius, 0.0), horizontal_size);
        let right_rect = Rect::new(tr_outer + Point2D::new(-blur_diameter, side_radius),
                                   vertical_size);
        let bottom_rect = Rect::new(bl_outer + Point2D::new(side_radius, -blur_diameter),
                                    horizontal_size);
        let left_rect = Rect::new(tl_outer + Point2D::new(0.0, side_radius), vertical_size);

        self.add_axis_aligned_gradient(sort_key,
                                       &top_rect,
                                       box_bounds,
                                       clip_mode,
                                       clip_region,
                                       white_image,
                                       dummy_mask_image,
                                       raster_to_image_map,
                                       texture_cache,
                                       &[transparent, transparent, *color, *color]);
        self.add_axis_aligned_gradient(sort_key,
                                       &right_rect,
                                       box_bounds,
                                       clip_mode,
                                       clip_region,
                                       white_image,
                                       dummy_mask_image,
                                       raster_to_image_map,
                                       texture_cache,
                                       &[*color, transparent, transparent, *color]);
        self.add_axis_aligned_gradient(sort_key,
                                       &bottom_rect,
                                       box_bounds,
                                       clip_mode,
                                       clip_region,
                                       white_image,
                                       dummy_mask_image,
                                       raster_to_image_map,
                                       texture_cache,
                                       &[*color, *color, transparent, transparent]);
        self.add_axis_aligned_gradient(sort_key,
                                       &left_rect,
                                       box_bounds,
                                       clip_mode,
                                       clip_region,
                                       white_image,
                                       dummy_mask_image,
                                       raster_to_image_map,
                                       texture_cache,
                                       &[transparent, *color, *color, transparent]);

        // Fill the center area.
        self.add_rectangle(sort_key,
                           &Rect::new(tl_outer + Point2D::new(blur_diameter, blur_diameter),
                                      Size2D::new(rect.size.width - twice_blur_diameter,
                                                  rect.size.height - twice_blur_diameter)),
                           box_bounds,
                           clip_mode,
                           clip_region,
                           white_image,
                           dummy_mask_image,
                           raster_to_image_map,
                           texture_cache,
                           color);
    }

    #[inline]
    fn add_border_edge(&mut self,
                       sort_key: &DisplayItemKey,
                       rect: &Rect<f32>,
                       direction: BorderEdgeDirection,
                       color: &ColorF,
                       border_style: BorderStyle,
                       white_image: &TextureCacheItem,
                       dummy_mask_image: &TextureCacheItem,
                       raster_to_image_map: &HashMap<RasterItem, ImageID, DefaultState<FnvHasher>>,
                       texture_cache: &TextureCache) {
        // TODO: Check for zero width/height borders!
        if color.a <= 0.0 {
            return
        }

        match border_style {
            BorderStyle::Dashed => {
                let (extent, step) = match direction {
                    BorderEdgeDirection::Horizontal => {
                        (rect.size.width, rect.size.height * BORDER_DASH_SIZE)
                    }
                    BorderEdgeDirection::Vertical => {
                        (rect.size.height, rect.size.width * BORDER_DASH_SIZE)
                    }
                };
                let mut origin = 0.0;
                while origin < extent {
                    let dash_rect = match direction {
                        BorderEdgeDirection::Horizontal => {
                            Rect::new(Point2D::new(rect.origin.x + origin, rect.origin.y),
                                      Size2D::new(f32::min(step, extent - origin),
                                                  rect.size.height))
                        }
                        BorderEdgeDirection::Vertical => {
                            Rect::new(Point2D::new(rect.origin.x, rect.origin.y + origin),
                                      Size2D::new(rect.size.width,
                                                  f32::min(step, extent - origin)))
                        }
                    };
                    add_rectangle(&mut self.vertex_buffer,
                                  &mut self.render_items,
                                  sort_key,
                                  &dash_rect,
                                  color,
                                  white_image,
                                  dummy_mask_image);

                    origin += step + step;
                }
            }
            BorderStyle::Dotted => {
                let (extent, step) = match direction {
                    BorderEdgeDirection::Horizontal => (rect.size.width, rect.size.height),
                    BorderEdgeDirection::Vertical => (rect.size.height, rect.size.width),
                };
                let mut origin = 0.0;
                while origin < extent {
                    let (dot_rect, mask_radius) = match direction {
                        BorderEdgeDirection::Horizontal => {
                            (Rect::new(Point2D::new(rect.origin.x + origin, rect.origin.y),
                                       Size2D::new(f32::min(step, extent - origin),
                                                   rect.size.height)),
                             rect.size.height / 2.0)
                        }
                        BorderEdgeDirection::Vertical => {
                            (Rect::new(Point2D::new(rect.origin.x, rect.origin.y + origin),
                                       Size2D::new(rect.size.width,
                                                   f32::min(step, extent - origin))),
                             rect.size.width / 2.0)
                        }
                    };

                    let raster_op =
                        BorderRadiusRasterOp::create(&Size2D::new(mask_radius, mask_radius),
                                                     &Size2D::new(0.0, 0.0)).expect(
                        "Didn't find border radius mask for dashed border!");
                    let raster_item = RasterItem::BorderRadius(raster_op);
                    let raster_item_id = raster_to_image_map[&raster_item];
                    let mask_image = texture_cache.get(raster_item_id);
                    let muv_rect = Rect::new(Point2D::new(mask_image.u0, mask_image.v0),
                                             Size2D::new(mask_image.u1 - mask_image.u0,
                                                         mask_image.v1 - mask_image.v0));

                    // Top left:
                    add_masked_rectangle(&mut self.vertex_buffer,
                                         &mut self.render_items,
                                         sort_key,
                                         &Rect::new(dot_rect.origin,
                                                    Size2D::new(dot_rect.size.width / 2.0,
                                                                dot_rect.size.height / 2.0)),
                                         &muv_rect,
                                         color,
                                         white_image,
                                         mask_image);
                    // Top right:
                    add_masked_rectangle(&mut self.vertex_buffer,
                                         &mut self.render_items,
                                         sort_key,
                                         &Rect::new(dot_rect.top_right(),
                                                    Size2D::new(-dot_rect.size.width / 2.0,
                                                                dot_rect.size.height / 2.0)),
                                         &muv_rect,
                                         color,
                                         white_image,
                                         mask_image);
                    // Bottom right:
                    add_masked_rectangle(&mut self.vertex_buffer,
                                         &mut self.render_items,
                                         sort_key,
                                         &Rect::new(dot_rect.bottom_right(),
                                                    Size2D::new(-dot_rect.size.width / 2.0,
                                                                -dot_rect.size.height / 2.0)),
                                         &muv_rect,
                                         color,
                                         white_image,
                                         mask_image);
                    // Bottom left:
                    add_masked_rectangle(&mut self.vertex_buffer,
                                         &mut self.render_items,
                                         sort_key,
                                         &Rect::new(dot_rect.bottom_left(),
                                                    Size2D::new(dot_rect.size.width / 2.0,
                                                                -dot_rect.size.height / 2.0)),
                                         &muv_rect,
                                         color,
                                         white_image,
                                         mask_image);

                    origin += step + step;
                }
            }
            BorderStyle::Double => {
                let (outer_rect, inner_rect) = match direction {
                    BorderEdgeDirection::Horizontal => {
                        (Rect::new(rect.origin,
                                   Size2D::new(rect.size.width, rect.size.height / 3.0)),
                         Rect::new(Point2D::new(rect.origin.x,
                                                rect.origin.y + rect.size.height * 2.0 / 3.0),
                                   Size2D::new(rect.size.width, rect.size.height / 3.0)))
                    }
                    BorderEdgeDirection::Vertical => {
                        (Rect::new(rect.origin,
                                   Size2D::new(rect.size.width / 3.0, rect.size.height)),
                         Rect::new(Point2D::new(rect.origin.x + rect.size.width * 2.0 / 3.0,
                                                rect.origin.y),
                                   Size2D::new(rect.size.width / 3.0, rect.size.height)))
                    }
                };
                add_rectangle(&mut self.vertex_buffer,
                              &mut self.render_items,
                              sort_key,
                              &outer_rect,
                              color,
                              white_image,
                              dummy_mask_image);
                add_rectangle(&mut self.vertex_buffer,
                              &mut self.render_items,
                              sort_key,
                              &inner_rect,
                              color,
                              white_image,
                              dummy_mask_image);
            }
            _ => {
                add_rectangle(&mut self.vertex_buffer,
                              &mut self.render_items,
                              sort_key,
                              rect,
                              color,
                              white_image,
                              dummy_mask_image);
            }
        }

        fn add_rectangle(vertex_buffer: &mut VertexBuffer,
                         render_items: &mut Vec<RenderItem>,
                         sort_key: &DisplayItemKey,
                         rect: &Rect<f32>,
                         color: &ColorF,
                         white_image: &TextureCacheItem,
                         mask_image: &TextureCacheItem) {
            let item = RenderItem {
                sort_key: sort_key.clone(),
                info: RenderItemInfo::Draw(DrawRenderItem {
                    color_texture_id: white_image.texture_id,
                    mask_texture_id: mask_image.texture_id,
                    primitive: Primitive::Rectangles,
                    first_vertex: vertex_buffer.len(),
                    vertex_count: 4,
                }),
            };

            vertex_buffer.push_white(rect.origin.x, rect.origin.y, color);
            vertex_buffer.push_white(rect.max_x(), rect.origin.y, color);
            vertex_buffer.push_white(rect.origin.x, rect.max_y(), color);
            vertex_buffer.push_white(rect.max_x(), rect.max_y(), color);

            render_items.push(item);
        }

        fn add_masked_rectangle(vertex_buffer: &mut VertexBuffer,
                                render_items: &mut Vec<RenderItem>,
                                sort_key: &DisplayItemKey,
                                rect: &Rect<f32>,
                                muv_rect: &Rect<f32>,
                                color: &ColorF,
                                white_image: &TextureCacheItem,
                                mask_image: &TextureCacheItem) {
            let item = RenderItem {
                sort_key: sort_key.clone(),
                info: RenderItemInfo::Draw(DrawRenderItem {
                    color_texture_id: white_image.texture_id,
                    mask_texture_id: mask_image.texture_id,
                    primitive: Primitive::Rectangles,
                    first_vertex: vertex_buffer.len(),
                    vertex_count: 4,
                }),
            };

            vertex_buffer.push_masked(rect.origin.x, rect.origin.y,
                                      color,
                                      muv_rect.origin.x, muv_rect.origin.y);
            vertex_buffer.push_masked(rect.max_x(), rect.origin.y,
                                      color,
                                      muv_rect.max_x(), muv_rect.origin.y);
            vertex_buffer.push_masked(rect.origin.x, rect.max_y(),
                                      color,
                                      muv_rect.origin.x, muv_rect.max_y());
            vertex_buffer.push_masked(rect.max_x(), rect.max_y(),
                                      color,
                                      muv_rect.max_x(), muv_rect.max_y());

            render_items.push(item);
        }
    }

    #[inline]
    fn add_border_corner(&mut self,
                         sort_key: &DisplayItemKey,
                         v0: Point2D<f32>,
                         v1: Point2D<f32>,
                         color0: &ColorF,
                         color1: &ColorF,
                         outer_radius: &Size2D<f32>,
                         inner_radius: &Size2D<f32>,
                         white_image: &TextureCacheItem,
                         dummy_mask_image: &TextureCacheItem,
                         raster_to_image_map: &HashMap<RasterItem, ImageID, DefaultState<FnvHasher>>,
                         texture_cache: &TextureCache) {
        // TODO: Check for zero width/height borders!
        let mask_image = match BorderRadiusRasterOp::create(outer_radius, inner_radius) {
            Some(raster_item) => {
                let raster_item = RasterItem::BorderRadius(raster_item);
                let raster_item_id = raster_to_image_map[&raster_item];
                texture_cache.get(raster_item_id)
            }
            None => {
                dummy_mask_image
            }
        };

        self.add_masked_rectangle(sort_key,
                                  &v0,
                                  &v1,
                                  &MAX_RECT,
                                  BoxShadowClipMode::None,
                                  color0,
                                  color1,
                                  &white_image,
                                  &mask_image);
    }

    fn add_masked_rectangle(&mut self,
                            sort_key: &DisplayItemKey,
                            v0: &Point2D<f32>,
                            v1: &Point2D<f32>,
                            clip: &Rect<f32>,
                            clip_mode: BoxShadowClipMode,
                            color0: &ColorF,
                            color1: &ColorF,
                            white_image: &TextureCacheItem,
                            mask_image: &TextureCacheItem) {
        if color0.a <= 0.0 || color1.a <= 0.0 {
            return
        }

        let vertices_rect = Rect::new(*v0, Size2D::new(v1.x - v0.x, v1.y - v0.y));
        let mask_uv_rect = Rect::new(Point2D::new(mask_image.u0, mask_image.v0),
                                     Size2D::new(mask_image.u1 - mask_image.u0,
                                                 mask_image.v1 - mask_image.v0));
        for clip_result in clipper::clip_rect_with_mode_pos_uv(&vertices_rect,
                                                               &mask_uv_rect,
                                                               clip,
                                                               clip_mode) {
            let item = RenderItem {
                sort_key: sort_key.clone(),
                info: RenderItemInfo::Draw(DrawRenderItem {
                    color_texture_id: white_image.texture_id,
                    mask_texture_id: mask_image.texture_id,
                    primitive: Primitive::Rectangles,
                    first_vertex: self.vertex_buffer.len(),
                    vertex_count: 4,
                }),
            };

            self.vertex_buffer.push_masked(clip_result.x0,
                                           clip_result.y0,
                                           color0,
                                           clip_result.u0,
                                           clip_result.v0);
            self.vertex_buffer.push_masked(clip_result.x1,
                                           clip_result.y0,
                                           color0,
                                           clip_result.u1,
                                           clip_result.v0);
            self.vertex_buffer.push_masked(clip_result.x0,
                                           clip_result.y1,
                                           color1,
                                           clip_result.u0,
                                           clip_result.v1);
            self.vertex_buffer.push_masked(clip_result.x1,
                                           clip_result.y1,
                                           color1,
                                           clip_result.u1,
                                           clip_result.v1);

            self.render_items.push(item);
        }
    }

    fn add_border(&mut self,
                  sort_key: &DisplayItemKey,
                  rect: &Rect<f32>,
                  info: &BorderDisplayItem,
                  white_image: &TextureCacheItem,
                  dummy_mask_image: &TextureCacheItem,
                  raster_to_image_map: &HashMap<RasterItem, ImageID, DefaultState<FnvHasher>>,
                  texture_cache: &TextureCache) {
        // TODO: If any border segment is alpha, place all in alpha pass.
        //       Is it ever worth batching at a per-segment level?
        let radius = &info.radius;
        let left = &info.left;
        let right = &info.right;
        let top = &info.top;
        let bottom = &info.bottom;

        let tl_outer = Point2D::new(rect.origin.x, rect.origin.y);
        let tl_inner = tl_outer + Point2D::new(radius.top_left.width.max(left.width),
                                               radius.top_left.height.max(top.width));

        let tr_outer = Point2D::new(rect.origin.x + rect.size.width, rect.origin.y);
        let tr_inner = tr_outer + Point2D::new(-radius.top_right.width.max(right.width),
                                               radius.top_right.height.max(top.width));

        let bl_outer = Point2D::new(rect.origin.x, rect.origin.y + rect.size.height);
        let bl_inner = bl_outer + Point2D::new(radius.bottom_left.width.max(left.width),
                                               -radius.bottom_left.height.max(bottom.width));

        let br_outer = Point2D::new(rect.origin.x + rect.size.width,
                                    rect.origin.y + rect.size.height);
        let br_inner = br_outer - Point2D::new(radius.bottom_right.width.max(right.width),
                                               radius.bottom_right.height.max(bottom.width));

        let left_color = left.border_color(1.0, 2.0/3.0, 0.3, 0.7);
        let top_color = top.border_color(1.0, 2.0/3.0, 0.3, 0.7);
        let right_color = right.border_color(2.0/3.0, 1.0, 0.7, 0.3);
        let bottom_color = bottom.border_color(2.0/3.0, 1.0, 0.7, 0.3);

        // Edges
        self.add_border_edge(sort_key,
                             &Rect::new(Point2D::new(tl_outer.x, tl_inner.y),
                                        Size2D::new(left.width, bl_inner.y - tl_inner.y)),
                             BorderEdgeDirection::Vertical,
                             &left_color,
                             info.left.style,
                             white_image,
                             dummy_mask_image,
                             raster_to_image_map,
                             texture_cache);

        self.add_border_edge(sort_key,
                             &Rect::new(Point2D::new(tl_inner.x, tl_outer.y),
                                        Size2D::new(tr_inner.x - tl_inner.x,
                                                    tr_outer.y + top.width - tl_outer.y)),
                             BorderEdgeDirection::Horizontal,
                             &top_color,
                             info.top.style,
                             white_image,
                             dummy_mask_image,
                             raster_to_image_map,
                             texture_cache);

        self.add_border_edge(sort_key,
                             &Rect::new(Point2D::new(br_outer.x - right.width, tr_inner.y),
                                        Size2D::new(right.width, br_inner.y - tr_inner.y)),
                             BorderEdgeDirection::Vertical,
                             &right_color,
                             info.right.style,
                             white_image,
                             dummy_mask_image,
                             raster_to_image_map,
                             texture_cache);

        self.add_border_edge(sort_key,
                             &Rect::new(Point2D::new(bl_inner.x, bl_outer.y - bottom.width),
                                        Size2D::new(br_inner.x - bl_inner.x,
                                                    br_outer.y - bl_outer.y + bottom.width)),
                             BorderEdgeDirection::Horizontal,
                             &bottom_color,
                             info.bottom.style,
                             white_image,
                             dummy_mask_image,
                             raster_to_image_map,
                             texture_cache);

        // Corners
        self.add_border_corner(sort_key,
                               tl_outer,
                               tl_inner,
                               &left_color,
                               &top_color,
                               &radius.top_left,
                               &info.top_left_inner_radius(),
                               white_image,
                               dummy_mask_image,
                               raster_to_image_map,
                               texture_cache);

        self.add_border_corner(sort_key,
                               tr_outer,
                               tr_inner,
                               &right_color,
                               &top_color,
                               &radius.top_right,
                               &info.top_right_inner_radius(),
                               white_image,
                               dummy_mask_image,
                               raster_to_image_map,
                               texture_cache);

        self.add_border_corner(sort_key,
                               br_outer,
                               br_inner,
                               &right_color,
                               &bottom_color,
                               &radius.bottom_right,
                               &info.bottom_right_inner_radius(),
                               white_image,
                               dummy_mask_image,
                               raster_to_image_map,
                               texture_cache);

        self.add_border_corner(sort_key,
                               bl_outer,
                               bl_inner,
                               &left_color,
                               &bottom_color,
                               &radius.bottom_left,
                               &info.bottom_left_inner_radius(),
                               white_image,
                               dummy_mask_image,
                               raster_to_image_map,
                               texture_cache);
    }

    // FIXME(pcwalton): Assumes rectangles are well-formed with origin in TL
    fn add_box_shadow_corner(&mut self,
                             sort_key: &DisplayItemKey,
                             top_left: &Point2D<f32>,
                             bottom_right: &Point2D<f32>,
                             box_bounds: &Rect<f32>,
                             color: &ColorF,
                             blur_radius: f32,
                             border_radius: f32,
                             clip_mode: BoxShadowClipMode,
                             white_image: &TextureCacheItem,
                             dummy_mask_image: &TextureCacheItem,
                             raster_to_image_map:
                                &HashMap<RasterItem, ImageID, DefaultState<FnvHasher>>,
                             texture_cache: &TextureCache) {
        let mask_image = match BoxShadowCornerRasterOp::create(blur_radius, border_radius) {
            Some(raster_item) => {
                let raster_item = RasterItem::BoxShadowCorner(raster_item);
                let raster_item_id = raster_to_image_map[&raster_item];
                texture_cache.get(raster_item_id)
            }
            None => dummy_mask_image,
        };

        let clip_rect = match clip_mode {
            BoxShadowClipMode::Outset => *box_bounds,
            BoxShadowClipMode::None => MAX_RECT,
            BoxShadowClipMode::Inset => {
                // TODO(pcwalton): Implement this.
                MAX_RECT
            }
        };

        self.add_masked_rectangle(sort_key,
                                  top_left,
                                  bottom_right,
                                  &clip_rect,
                                  clip_mode,
                                  color,
                                  color,
                                  white_image,
                                  &mask_image)
    }
}

trait BuildRequiredResources {
    fn build_resource_list(&mut self, flat_draw_lists: &FlatDrawListArray);
}

impl BuildRequiredResources for AABBTreeNode {
    fn build_resource_list(&mut self, flat_draw_lists: &FlatDrawListArray) {
        //let _pf = util::ProfileScope::new("  build_resource_list");
        let mut resource_list = ResourceList::new();

        for item_key in &self.src_items {
            let display_item = flat_draw_lists.get_item(item_key);

            // Handle border radius for complex clipping regions.
            for complex_clip_region in display_item.clip.complex.iter() {
                resource_list.add_radius_raster(&complex_clip_region.radii.top_left,
                                                &Size2D::new(0.0, 0.0));
                resource_list.add_radius_raster(&complex_clip_region.radii.top_right,
                                                &Size2D::new(0.0, 0.0));
                resource_list.add_radius_raster(&complex_clip_region.radii.bottom_left,
                                                &Size2D::new(0.0, 0.0));
                resource_list.add_radius_raster(&complex_clip_region.radii.bottom_right,
                                                &Size2D::new(0.0, 0.0));
            }

            match display_item.item {
                SpecificDisplayItem::Image(ref info) => {
                    resource_list.add_image(info.image_id);
                }
                SpecificDisplayItem::Text(ref info) => {
                    for glyph in &info.glyphs {
                        let glyph = Glyph::new(info.size, info.blur_radius, glyph.index);
                        resource_list.add_glyph(info.font_id.clone(), glyph);
                    }
                }
                SpecificDisplayItem::Rectangle(..) => {}
                SpecificDisplayItem::Iframe(..) => {}
                SpecificDisplayItem::Gradient(..) => {}
                SpecificDisplayItem::Composite(..) => {}
                SpecificDisplayItem::BoxShadow(ref info) => {
                    resource_list.add_box_shadow_corner(info.blur_radius,
                                                        info.border_radius);
                }
                SpecificDisplayItem::Border(ref info) => {
                    resource_list.add_radius_raster(&info.radius.top_left,
                                                    &info.top_left_inner_radius());
                    resource_list.add_radius_raster(&info.radius.top_right,
                                                    &info.top_right_inner_radius());
                    resource_list.add_radius_raster(&info.radius.bottom_left,
                                                    &info.bottom_left_inner_radius());
                    resource_list.add_radius_raster(&info.radius.bottom_right,
                                                    &info.bottom_right_inner_radius());

                    if info.top.style == BorderStyle::Dotted {
                        resource_list.add_radius_raster(&Size2D::new(info.top.width / 2.0,
                                                                     info.top.width / 2.0),
                                                        &Size2D::new(0.0, 0.0));
                    }
                    if info.right.style == BorderStyle::Dotted {
                        resource_list.add_radius_raster(&Size2D::new(info.right.width / 2.0,
                                                                     info.right.width / 2.0),
                                                        &Size2D::new(0.0, 0.0));
                    }
                    if info.bottom.style == BorderStyle::Dotted {
                        resource_list.add_radius_raster(&Size2D::new(info.bottom.width / 2.0,
                                                                     info.bottom.width / 2.0),
                                                        &Size2D::new(0.0, 0.0));
                    }
                    if info.left.style == BorderStyle::Dotted {
                        resource_list.add_radius_raster(&Size2D::new(info.left.width / 2.0,
                                                                     info.left.width / 2.0),
                                                        &Size2D::new(0.0, 0.0));
                    }
                }
            }
        }

        self.resource_list = Some(resource_list);
    }
}

impl RenderBatch {
    fn new(batch_id: BatchId,
           sort_key: DisplayItemKey,
           program_id: ProgramId,
           color_texture_id: TextureId,
           mask_texture_id: TextureId) -> RenderBatch {
        RenderBatch {
            sort_key: sort_key,
            batch_id: batch_id,
            program_id: program_id,
            color_texture_id: color_texture_id,
            mask_texture_id: mask_texture_id,
            vertices: Vec::new(),
            indices: Vec::new(),
            matrix_map: HashMap::new(),
        }
    }

    fn can_add_to_batch(&self,
                        item: &DrawRenderItem,
                        key: &DisplayItemKey,
                        program_id: ProgramId) -> bool {
        let matrix_ok = self.matrix_map.len() < MAX_MATRICES_PER_BATCH ||
                        self.matrix_map.contains_key(&key.draw_list_index);

        program_id == self.program_id &&
            item.color_texture_id == self.color_texture_id &&
            item.mask_texture_id == self.mask_texture_id &&
            self.vertices.len() < 65535 &&                  // to ensure we can use u16 index buffers
            matrix_ok
    }

    fn add_draw_item(&mut self,
                     item: &DrawRenderItem,
                     vertex_buffer: &Vec<WorkVertex>,
                     key: &DisplayItemKey,
                     device_pixel_ratio: f32) {
        debug_assert!(item.color_texture_id == self.color_texture_id);
        debug_assert!(item.mask_texture_id == self.mask_texture_id);

        let next_matrix_index = self.matrix_map.len() as u8;
        let matrix_index = match self.matrix_map.entry(key.draw_list_index) {
            Vacant(entry) => *entry.insert(next_matrix_index),
            Occupied(entry) => *entry.get(),
        };
        debug_assert!(self.matrix_map.len() <= MAX_MATRICES_PER_BATCH);

        let index_offset = self.vertices.len();

        match item.primitive {
            Primitive::Rectangles | Primitive::Glyphs => {
                for i in (0..item.vertex_count as usize).step_by(4) {
                    let index_base = (index_offset + i) as u16;
                    self.indices.push(index_base + 0);
                    self.indices.push(index_base + 1);
                    self.indices.push(index_base + 2);
                    self.indices.push(index_base + 2);
                    self.indices.push(index_base + 3);
                    self.indices.push(index_base + 1);
                }
            }
            Primitive::TriangleFan => {
                for i in (1..item.vertex_count as usize - 1) {
                    self.indices.push(index_offset as u16);        // center vertex
                    self.indices.push((index_offset + i + 0) as u16);
                    self.indices.push((index_offset + i + 1) as u16);
                }
            }
        }

        for i in 0..item.vertex_count {
            let vertex_index = (item.first_vertex + i) as usize;
            let src_vertex = &vertex_buffer[vertex_index];
            self.vertices.push(PackedVertex::new(src_vertex,
                                                 device_pixel_ratio,
                                                 matrix_index));
        }
    }
}

trait BorderSideHelpers {
    fn border_color(&self,
                    scale_factor_0: f32,
                    scale_factor_1: f32,
                    black_color_0: f32,
                    black_color_1: f32) -> ColorF;
}

impl BorderSideHelpers for BorderSide {
    fn border_color(&self,
                    scale_factor_0: f32,
                    scale_factor_1: f32,
                    black_color_0: f32,
                    black_color_1: f32) -> ColorF {
        match self.style {
            BorderStyle::Inset => {
                if self.color.r != 0.0 || self.color.g != 0.0 || self.color.b != 0.0 {
                    self.color.scale_rgb(scale_factor_1)
                } else {
                    ColorF::new(black_color_1, black_color_1, black_color_1, self.color.a)
                }
            }
            BorderStyle::Outset => {
                if self.color.r != 0.0 || self.color.g != 0.0 || self.color.b != 0.0 {
                    self.color.scale_rgb(scale_factor_0)
                } else {
                    ColorF::new(black_color_0, black_color_0, black_color_0, self.color.a)
                }
            }
            _ => self.color,
        }
    }
}

fn mask_for_border_radius<'a>(dummy_mask_image: &'a TextureCacheItem,
                              raster_to_image_map: &HashMap<RasterItem,
                                                            ImageID,
                                                            DefaultState<FnvHasher>>,
                              texture_cache: &'a TextureCache,
                              border_radius: f32)
                              -> &'a TextureCacheItem {
    if border_radius == 0.0 {
        return dummy_mask_image
    }

    let border_radius = Au::from_f32_px(border_radius);
    match raster_to_image_map.get(&RasterItem::BorderRadius(BorderRadiusRasterOp {
        outer_radius_x: border_radius,
        outer_radius_y: border_radius,
        inner_radius_x: Au(0),
        inner_radius_y: Au(0),
    })) {
        Some(image_info) => texture_cache.get(*image_info),
        None => panic!("Couldn't find border radius {:?} in raster-to-image map!", border_radius),
    }
}

