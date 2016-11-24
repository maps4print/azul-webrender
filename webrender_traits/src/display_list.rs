/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use app_units::Au;
use euclid::{Matrix4D, Point2D, Rect, Size2D};
use std::mem;
use std::slice;
use {AuxiliaryLists, AuxiliaryListsDescriptor, BorderDisplayItem, BorderRadius};
use {BorderSide, BoxShadowClipMode, BoxShadowDisplayItem, BuiltDisplayList};
use {BuiltDisplayListDescriptor, ClipRegion, ComplexClipRegion, ColorF};
use {DisplayItem, DisplayListMode, FilterOp, YuvColorSpace};
use {FontKey, GlyphInstance, GradientDisplayItem, GradientStop, IframeDisplayItem};
use {ImageDisplayItem, ImageKey, ImageMask, ImageRendering, ItemRange, MixBlendMode, PipelineId};
use {PushScrollLayerItem, PushStackingContextDisplayItem, RectangleDisplayItem, ScrollLayerId};
use {ScrollPolicy, SpecificDisplayItem, StackingContext, TextDisplayItem, WebGLContextId};
use {WebGLDisplayItem, YuvImageDisplayItem};

impl BuiltDisplayListDescriptor {
    pub fn size(&self) -> usize {
        self.display_list_items_size + self.display_items_size
    }
}

impl BuiltDisplayList {
    pub fn from_data(data: Vec<u8>, descriptor: BuiltDisplayListDescriptor) -> BuiltDisplayList {
        BuiltDisplayList {
            data: data,
            descriptor: descriptor,
        }
    }

    pub fn data(&self) -> &[u8] {
        &self.data[..]
    }

    pub fn descriptor(&self) -> &BuiltDisplayListDescriptor {
        &self.descriptor
    }

    pub fn all_display_items<'a>(&'a self) -> &'a [DisplayItem] {
        unsafe {
            convert_blob_to_pod(&self.data[0..self.descriptor.display_list_items_size])
        }
    }
}

pub struct DisplayListBuilder {
    pub mode: DisplayListMode,
    pub list: Vec<DisplayItem>,
    auxiliary_lists_builder: AuxiliaryListsBuilder,
}

impl DisplayListBuilder {
    pub fn new() -> DisplayListBuilder {
        DisplayListBuilder {
            mode: DisplayListMode::Default,
            list: Vec::new(),
            auxiliary_lists_builder: AuxiliaryListsBuilder::new(),
        }
    }

    pub fn print_display_list(&mut self) {
        for item in &self.list {
            println!("{:?}", item);
        }
    }

    pub fn push_rect(&mut self,
                     rect: Rect<f32>,
                     clip: ClipRegion,
                     color: ColorF) {
        let item = RectangleDisplayItem {
            color: color,
        };

        let display_item = DisplayItem {
            item: SpecificDisplayItem::Rectangle(item),
            rect: rect,
            clip: clip,
        };

        self.list.push(display_item);
    }

    pub fn push_image(&mut self,
                      rect: Rect<f32>,
                      clip: ClipRegion,
                      stretch_size: Size2D<f32>,
                      tile_spacing: Size2D<f32>,
                      image_rendering: ImageRendering,
                      key: ImageKey) {
        let item = ImageDisplayItem {
            image_key: key,
            stretch_size: stretch_size,
            tile_spacing: tile_spacing,
            image_rendering: image_rendering,
        };

        let display_item = DisplayItem {
            item: SpecificDisplayItem::Image(item),
            rect: rect,
            clip: clip,
        };

        self.list.push(display_item);
    }

    pub fn push_yuv_image(&mut self,
                          rect: Rect<f32>,
                          clip: ClipRegion,
                          y_key: ImageKey,
                          u_key: ImageKey,
                          v_key: ImageKey,
                          color_space: YuvColorSpace) {
        self.list.push(DisplayItem {
            item: SpecificDisplayItem::YuvImage(YuvImageDisplayItem {
                y_image_key: y_key,
                u_image_key: u_key,
                v_image_key: v_key,
                color_space: color_space,
            }),
            rect: rect,
            clip: clip,
        });
    }

    pub fn push_webgl_canvas(&mut self,
                             rect: Rect<f32>,
                             clip: ClipRegion,
                             context_id: WebGLContextId) {
        let item = WebGLDisplayItem {
            context_id: context_id,
        };

        let display_item = DisplayItem {
            item: SpecificDisplayItem::WebGL(item),
            rect: rect,
            clip: clip,
        };

        self.list.push(display_item);
    }

    pub fn push_text(&mut self,
                     rect: Rect<f32>,
                     clip: ClipRegion,
                     glyphs: Vec<GlyphInstance>,
                     font_key: FontKey,
                     color: ColorF,
                     size: Au,
                     blur_radius: Au) {
        // Sanity check - anything with glyphs bigger than this
        // is probably going to consume too much memory to render
        // efficiently anyway. This is specifically to work around
        // the font_advance.html reftest, which creates a very large
        // font as a crash test - the rendering is also ignored
        // by the azure renderer.
        if size < Au::from_px(4096) {
            let item = TextDisplayItem {
                color: color,
                glyphs: self.auxiliary_lists_builder.add_glyph_instances(&glyphs),
                font_key: font_key,
                size: size,
                blur_radius: blur_radius,
            };

            let display_item = DisplayItem {
                item: SpecificDisplayItem::Text(item),
                rect: rect,
                clip: clip,
            };

            self.list.push(display_item);
        }
    }

    pub fn push_border(&mut self,
                       rect: Rect<f32>,
                       clip: ClipRegion,
                       left: BorderSide,
                       top: BorderSide,
                       right: BorderSide,
                       bottom: BorderSide,
                       radius: BorderRadius) {
        let item = BorderDisplayItem {
            left: left,
            top: top,
            right: right,
            bottom: bottom,
            radius: radius,
        };

        let display_item = DisplayItem {
            item: SpecificDisplayItem::Border(item),
            rect: rect,
            clip: clip,
        };

        self.list.push(display_item);
    }

    pub fn push_box_shadow(&mut self,
                           rect: Rect<f32>,
                           clip: ClipRegion,
                           box_bounds: Rect<f32>,
                           offset: Point2D<f32>,
                           color: ColorF,
                           blur_radius: f32,
                           spread_radius: f32,
                           border_radius: f32,
                           clip_mode: BoxShadowClipMode) {
        let item = BoxShadowDisplayItem {
            box_bounds: box_bounds,
            offset: offset,
            color: color,
            blur_radius: blur_radius,
            spread_radius: spread_radius,
            border_radius: border_radius,
            clip_mode: clip_mode,
        };

        let display_item = DisplayItem {
            item: SpecificDisplayItem::BoxShadow(item),
            rect: rect,
            clip: clip,
        };

        self.list.push(display_item);
    }

    pub fn push_gradient(&mut self,
                         rect: Rect<f32>,
                         clip: ClipRegion,
                         start_point: Point2D<f32>,
                         end_point: Point2D<f32>,
                         stops: Vec<GradientStop>) {
        let item = GradientDisplayItem {
            start_point: start_point,
            end_point: end_point,
            stops: self.auxiliary_lists_builder.add_gradient_stops(&stops),
        };

        let display_item = DisplayItem {
            item: SpecificDisplayItem::Gradient(item),
            rect: rect,
            clip: clip,
        };

        self.list.push(display_item);
    }

    pub fn push_stacking_context(&mut self, 
                                 scroll_policy: ScrollPolicy,
                                 bounds: Rect<f32>,
                                 overflow: Rect<f32>,
                                 z_index: i32,
                                 transform: &Matrix4D<f32>,
                                 perspective: &Matrix4D<f32>,
                                 mix_blend_mode: MixBlendMode,
                                 filters: Vec<FilterOp>) {
        let stacking_context = StackingContext {
            scroll_policy: scroll_policy,
            bounds: bounds,
            overflow: overflow,
            z_index: z_index,
            transform: transform.clone(),
            perspective: perspective.clone(),
            mix_blend_mode: mix_blend_mode,
            filters: self.auxiliary_lists_builder.add_filters(&filters),
        };

        let item = DisplayItem {
            item: SpecificDisplayItem::PushStackingContext(PushStackingContextDisplayItem {
                stacking_context: stacking_context
            }),
            rect: Rect::zero(),
            clip: ClipRegion::simple(&Rect::zero()),
        };
        self.list.push(item);
    }

    pub fn pop_stacking_context(&mut self) {
        let item = DisplayItem {
            item: SpecificDisplayItem::PopStackingContext,
            rect: Rect::zero(),
            clip: ClipRegion::simple(&Rect::zero()),
        };
        self.list.push(item);
    }

    pub fn push_scroll_layer(&mut self,
                             clip: Rect<f32>,
                             content_size: Size2D<f32>,
                             id: ScrollLayerId) {
        let item = PushScrollLayerItem {
            content_size: content_size,
            id: id,
        };

        let item = DisplayItem {
            item: SpecificDisplayItem::PushScrollLayer(item),
            rect: clip,
            clip: ClipRegion::simple(&Rect::zero()),
        };
        self.list.push(item);
    }

    pub fn pop_scroll_layer(&mut self) {
        let item = DisplayItem {
            item: SpecificDisplayItem::PopScrollLayer,
            rect: Rect::zero(),
            clip: ClipRegion::simple(&Rect::zero()),
        };
        self.list.push(item);
    }

    pub fn push_iframe(&mut self, rect: Rect<f32>, clip: ClipRegion, pipeline_id: PipelineId) {
        let item = DisplayItem {
            item: SpecificDisplayItem::Iframe(IframeDisplayItem { pipeline_id: pipeline_id }),
            rect: rect,
            clip: clip,
        };
        self.list.push(item);
    }

    pub fn new_clip_region(&mut self,
                           rect: &Rect<f32>,
                           complex: Vec<ComplexClipRegion>,
                           image_mask: Option<ImageMask>)
                           -> ClipRegion {
        ClipRegion::new(rect, complex, image_mask, &mut self.auxiliary_lists_builder)
    }

    pub fn finalize(self) -> (BuiltDisplayList, AuxiliaryLists) {
        unsafe {
            let blob = convert_pod_to_blob(&self.list).to_vec();
            let display_list_items_size = blob.len();

            (BuiltDisplayList {
                 descriptor: BuiltDisplayListDescriptor {
                     mode: self.mode,
                     display_list_items_size: display_list_items_size,
                     display_items_size: 0,
                 },
                 data: blob,
             },
             self.auxiliary_lists_builder.finalize())
        }
    }
}

impl ItemRange {
    pub fn new<T>(backing_list: &mut Vec<T>, items: &[T]) -> ItemRange where T: Copy + Clone {
        let start = backing_list.len();
        backing_list.extend_from_slice(items);
        ItemRange {
            start: start,
            length: items.len(),
        }
    }

    pub fn empty() -> ItemRange {
        ItemRange {
            start: 0,
            length: 0,
        }
    }

    pub fn get<'a, T>(&self, backing_list: &'a [T]) -> &'a [T] {
        &backing_list[self.start..(self.start + self.length)]
    }

    pub fn get_mut<'a, T>(&self, backing_list: &'a mut [T]) -> &'a mut [T] {
        &mut backing_list[self.start..(self.start + self.length)]
    }
}

#[derive(Clone)]
pub struct AuxiliaryListsBuilder {
    gradient_stops: Vec<GradientStop>,
    complex_clip_regions: Vec<ComplexClipRegion>,
    filters: Vec<FilterOp>,
    glyph_instances: Vec<GlyphInstance>,
}

impl AuxiliaryListsBuilder {
    pub fn new() -> AuxiliaryListsBuilder {
        AuxiliaryListsBuilder {
            gradient_stops: Vec::new(),
            complex_clip_regions: Vec::new(),
            filters: Vec::new(),
            glyph_instances: Vec::new(),
        }
    }

    pub fn add_gradient_stops(&mut self, gradient_stops: &[GradientStop]) -> ItemRange {
        ItemRange::new(&mut self.gradient_stops, gradient_stops)
    }

    pub fn gradient_stops(&self, gradient_stops_range: &ItemRange) -> &[GradientStop] {
        gradient_stops_range.get(&self.gradient_stops[..])
    }

    pub fn add_complex_clip_regions(&mut self, complex_clip_regions: &[ComplexClipRegion])
                                    -> ItemRange {
        ItemRange::new(&mut self.complex_clip_regions, complex_clip_regions)
    }

    pub fn complex_clip_regions(&self, complex_clip_regions_range: &ItemRange)
                                -> &[ComplexClipRegion] {
        complex_clip_regions_range.get(&self.complex_clip_regions[..])
    }

    pub fn add_filters(&mut self, filters: &[FilterOp]) -> ItemRange {
        ItemRange::new(&mut self.filters, filters)
    }

    pub fn filters(&self, filters_range: &ItemRange) -> &[FilterOp] {
        filters_range.get(&self.filters[..])
    }

    pub fn add_glyph_instances(&mut self, glyph_instances: &[GlyphInstance]) -> ItemRange {
        ItemRange::new(&mut self.glyph_instances, glyph_instances)
    }

    pub fn glyph_instances(&self, glyph_instances_range: &ItemRange) -> &[GlyphInstance] {
        glyph_instances_range.get(&self.glyph_instances[..])
    }

    pub fn finalize(self) -> AuxiliaryLists {
        unsafe {
            let mut blob = convert_pod_to_blob(&self.gradient_stops).to_vec();
            let gradient_stops_size = blob.len();
            blob.extend_from_slice(convert_pod_to_blob(&self.complex_clip_regions));
            let complex_clip_regions_size = blob.len() - gradient_stops_size;
            blob.extend_from_slice(convert_pod_to_blob(&self.filters));
            let filters_size = blob.len() - (complex_clip_regions_size + gradient_stops_size);
            blob.extend_from_slice(convert_pod_to_blob(&self.glyph_instances));
            let glyph_instances_size = blob.len() -
                (complex_clip_regions_size + gradient_stops_size + filters_size);

            AuxiliaryLists {
                data: blob,
                descriptor: AuxiliaryListsDescriptor {
                    gradient_stops_size: gradient_stops_size,
                    complex_clip_regions_size: complex_clip_regions_size,
                    filters_size: filters_size,
                    glyph_instances_size: glyph_instances_size,
                },
            }
        }
    }
}

impl AuxiliaryListsDescriptor {
    pub fn size(&self) -> usize {
        self.gradient_stops_size + self.complex_clip_regions_size + self.filters_size +
            self.glyph_instances_size
    }
}

impl AuxiliaryLists {
    /// Creates a new `AuxiliaryLists` instance from a descriptor and data received over a channel.
    pub fn from_data(data: Vec<u8>, descriptor: AuxiliaryListsDescriptor) -> AuxiliaryLists {
        AuxiliaryLists {
            data: data,
            descriptor: descriptor,
        }
    }

    pub fn data(&self) -> &[u8] {
        &self.data[..]
    }

    pub fn descriptor(&self) -> &AuxiliaryListsDescriptor {
        &self.descriptor
    }

    /// Returns the gradient stops described by `gradient_stops_range`.
    pub fn gradient_stops(&self, gradient_stops_range: &ItemRange) -> &[GradientStop] {
        unsafe {
            let end = self.descriptor.gradient_stops_size;
            gradient_stops_range.get(convert_blob_to_pod(&self.data[0..end]))
        }
    }

    /// Returns the complex clipping regions described by `complex_clip_regions_range`.
    pub fn complex_clip_regions(&self, complex_clip_regions_range: &ItemRange)
                                -> &[ComplexClipRegion] {
        let start = self.descriptor.gradient_stops_size;
        let end = start + self.descriptor.complex_clip_regions_size;
        unsafe {
            complex_clip_regions_range.get(convert_blob_to_pod(&self.data[start..end]))
        }
    }

    /// Returns the filters described by `filters_range`.
    pub fn filters(&self, filters_range: &ItemRange) -> &[FilterOp] {
        let start = self.descriptor.gradient_stops_size +
            self.descriptor.complex_clip_regions_size;
        let end = start + self.descriptor.filters_size;
        unsafe {
            filters_range.get(convert_blob_to_pod(&self.data[start..end]))
        }
    }

    /// Returns the glyph instances described by `glyph_instances_range`.
    pub fn glyph_instances(&self, glyph_instances_range: &ItemRange) -> &[GlyphInstance] {
        let start = self.descriptor.gradient_stops_size +
            self.descriptor.complex_clip_regions_size + self.descriptor.filters_size;
        unsafe {
            glyph_instances_range.get(convert_blob_to_pod(&self.data[start..]))
        }
    }
}

unsafe fn convert_pod_to_blob<T>(data: &[T]) -> &[u8] where T: Copy + 'static {
    slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * mem::size_of::<T>())
}

unsafe fn convert_blob_to_pod<T>(blob: &[u8]) -> &[T] where T: Copy + 'static {
    slice::from_raw_parts(blob.as_ptr() as *const T, blob.len() / mem::size_of::<T>())
}

