/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Servo heavily uses display lists, which are retained-mode lists of painting commands to
//! perform. Using a list instead of painting elements in immediate mode allows transforms, hit
//! testing, and invalidation to be performed using the same primitives as painting. It also allows
//! Servo to aggressively cull invisible and out-of-bounds painting elements, to reduce overdraw.
//! Finally, display lists allow tiles to be farmed out onto multiple CPUs and painted in parallel
//! (although this benefit does not apply to GPU-based painting).
//!
//! Display items describe relatively high-level drawing operations (for example, entire borders
//! and shadows instead of lines and blur operations), to reduce the amount of allocation required.
//! They are therefore not exactly analogous to constructs like Skia pictures, which consist of
//! low-level drawing primitives.

use app_units::Au;
use azure::azure::AzFloat;
use azure::azure_hl::Color;
use euclid::{Matrix4D, Point2D, Rect, Size2D};
use euclid::approxeq::ApproxEq;
use euclid::num::{One, Zero};
use euclid::rect::TypedRect;
use euclid::side_offsets::SideOffsets2D;
use gfx_traits::{LayerId, ScrollPolicy, StackingContextId};
use gfx_traits::print_tree::PrintTree;
use ipc_channel::ipc::IpcSharedMemory;
use msg::constellation_msg::PipelineId;
use net_traits::image::base::{Image, PixelFormat};
use paint_context::PaintContext;
use range::Range;
use std::cmp::{self, Ordering};
use std::collections::HashMap;
use std::fmt;
use std::mem;
use std::sync::Arc;
use style::computed_values::{border_style, filter, image_rendering, mix_blend_mode};
use style_traits::cursor::Cursor;
use text::TextRun;
use text::glyph::ByteIndex;
use util::geometry::{self, ScreenPx, max_rect};
use webrender_traits::{self, WebGLContextId};

pub use style::dom::OpaqueNode;

// It seems cleaner to have layout code not mention Azure directly, so let's just reexport this for
// layout to use.
pub use azure::azure_hl::GradientStop;

/// The factor that we multiply the blur radius by in order to inflate the boundaries of display
/// items that involve a blur. This ensures that the display item boundaries include all the ink.
pub static BLUR_INFLATION_FACTOR: i32 = 3;

/// LayerInfo is used to store PaintLayer metadata during DisplayList construction.
/// It is also used for tracking LayerIds when creating layers to preserve ordering when
/// layered DisplayItems should render underneath unlayered DisplayItems.
#[derive(Clone, Copy, HeapSizeOf, Deserialize, Serialize, Debug)]
pub struct LayerInfo {
    /// The base LayerId of this layer.
    pub layer_id: LayerId,

    /// The scroll policy of this layer.
    pub scroll_policy: ScrollPolicy,

    /// The subpage that this layer represents, if there is one.
    pub subpage_pipeline_id: Option<PipelineId>,

    /// The id for the next layer in the sequence. This is used for synthesizing
    /// layers for content that needs to be displayed on top of this layer.
    pub next_layer_id: LayerId,

    /// The color of the background in this layer. Used for unpainted content.
    pub background_color: Color,
}

impl LayerInfo {
    pub fn new(id: LayerId,
               scroll_policy: ScrollPolicy,
               subpage_pipeline_id: Option<PipelineId>,
               background_color: Color)
               -> LayerInfo {
        LayerInfo {
            layer_id: id,
            scroll_policy: scroll_policy,
            subpage_pipeline_id: subpage_pipeline_id,
            next_layer_id: id.companion_layer_id(),
            background_color: background_color,
        }
    }
}

#[derive(HeapSizeOf, Deserialize, Serialize)]
pub struct DisplayList {
    pub list: Vec<DisplayItem>,
}

impl DisplayList {
    pub fn new(root_stacking_context: StackingContext,
               all_items: Vec<DisplayItem>)
               -> DisplayList {
        let mut mapped_items = HashMap::new();
        for item in all_items.into_iter() {
            let items = mapped_items.entry(item.stacking_context_id()).or_insert(Vec::new());
            items.push(item);
        }

        let mut list = Vec::new();
        DisplayList::generate_display_list(&mut list, &mut mapped_items, root_stacking_context);

        DisplayList {
            list: list,
        }
    }

    fn generate_display_list(list: &mut Vec<DisplayItem>,
                             mapped_items: &mut HashMap<StackingContextId, Vec<DisplayItem>>,
                             mut stacking_context: StackingContext) {
        let mut child_stacking_contexts =
            mem::replace(&mut stacking_context.children, Vec::new());
        child_stacking_contexts.sort();
        let mut child_stacking_contexts = child_stacking_contexts.into_iter().peekable();

        let mut child_items = mapped_items.remove(&stacking_context.id)
                                          .unwrap_or(Vec::new());
        child_items.sort_by(|a, b| a.base().section.cmp(&b.base().section));
        child_items.reverse();

        let stacking_context_id = stacking_context.id;
        let real_stacking_context = stacking_context.context_type == StackingContextType::Real;
        if real_stacking_context {
            list.push(DisplayItem::PushStackingContext(Box::new(PushStackingContextItem {
                base: BaseDisplayItem::empty(),
                stacking_context: stacking_context,
            })));
        }

        // Properly order display items that make up a stacking context. "Steps" here
        // refer to the steps in CSS 2.1 Appendix E.
        // Steps 1 and 2: Borders and background for the root.
        while child_items.last().map_or(false,
             |child| child.section() == DisplayListSection::BackgroundAndBorders) {
            list.push(child_items.pop().unwrap());
        }

        // Step 3: Positioned descendants with negative z-indices.
        while child_stacking_contexts.peek().map_or(false, |child| child.z_index < 0) {
            let context = child_stacking_contexts.next().unwrap();
            DisplayList::generate_display_list(list, mapped_items, context);
        }

        // Step 4: Block backgrounds and borders.
        while child_items.last().map_or(false,
             |child| child.section() == DisplayListSection::BlockBackgroundsAndBorders) {
            list.push(child_items.pop().unwrap());
        }

        // Step 5: Floats.
        while child_stacking_contexts.peek().map_or(false,
            |child| child.context_type == StackingContextType::PseudoFloat) {
            let context = child_stacking_contexts.next().unwrap();
            DisplayList::generate_display_list(list, mapped_items, context);
        }

        // Step 6 & 7: Content and inlines that generate stacking contexts.
        while child_items.last().map_or(false,
             |child| child.section() == DisplayListSection::Content) {
            list.push(child_items.pop().unwrap());
        }

        // Step 8 & 9: Positioned descendants with nonnegative, numeric z-indices.
        for child in child_stacking_contexts {
            DisplayList::generate_display_list(list, mapped_items, child);
        }

        // Step 10: Outlines.
        list.extend(child_items);

        if real_stacking_context {
            list.push(DisplayItem::PopStackingContext(Box::new(
                PopStackingContextItem {
                    base: BaseDisplayItem::empty(),
                    stacking_context_id: stacking_context_id,
                }
            )));
        }
    }

    /// Draws the DisplayList in order.
    pub fn draw_into_context<'a>(&self,
                                 paint_context: &mut PaintContext,
                                 transform: &Matrix4D<f32>,
                                 stacking_context_id: StackingContextId,
                                 start: usize,
                                 end: usize) {
        let mut traversal = DisplayListTraversal::new_partial(self,
                                                              stacking_context_id,
                                                              start,
                                                              end);
        self.draw_with_state(&mut traversal,
                             paint_context,
                             transform,
                             &Point2D::zero(),
                             None);
    }

    /// Draws a single DisplayItem into the given PaintContext.
    pub fn draw_item_at_index_into_context(&self,
                                           paint_context: &mut PaintContext,
                                           transform: &Matrix4D<f32>,
                                           index: usize) {
        let old_transform = paint_context.draw_target.get_transform();
        paint_context.draw_target.set_transform(&transform.to_2d());

        let item = &self.list[index];
        item.draw_into_context(paint_context);

        paint_context.draw_target.set_transform(&old_transform);
    }

    fn draw_with_state<'a>(&'a self,
                           traversal: &mut DisplayListTraversal,
                           paint_context: &mut PaintContext,
                           transform: &Matrix4D<f32>,
                           subpixel_offset: &Point2D<Au>,
                           tile_rect: Option<Rect<Au>>) {
        while let Some(item) = traversal.next() {
            match item {
                &DisplayItem::PushStackingContext(ref stacking_context_item) => {
                    let context = &stacking_context_item.stacking_context;
                    if context.intersects_rect_in_parent_context(tile_rect) {
                        self.draw_stacking_context(traversal,
                                                   context,
                                                   paint_context,
                                                   transform,
                                                   subpixel_offset);
                    } else {
                        traversal.skip_to_end_of_stacking_context(context.id);
                    }
                }
                &DisplayItem::PopStackingContext(_) => return,
                _ => {
                    if item.intersects_rect_in_parent_context(tile_rect) {
                        item.draw_into_context(paint_context);
                    }
                }
            }
        }
    }

    fn draw_stacking_context(&self,
                             traversal: &mut DisplayListTraversal,
                             stacking_context: &StackingContext,
                             paint_context: &mut PaintContext,
                             transform: &Matrix4D<f32>,
                             subpixel_offset: &Point2D<Au>) {
        debug_assert!(stacking_context.context_type == StackingContextType::Real);

        let draw_target = paint_context.get_or_create_temporary_draw_target(
            &stacking_context.filters,
            stacking_context.blend_mode);

        let old_transform = paint_context.draw_target.get_transform();
        let pixels_per_px = paint_context.screen_pixels_per_px();
        let (transform, subpixel_offset) = match stacking_context.layer_info {
            // If this stacking context starts a layer, the offset and
            // transformation are handled by layer position within the
            // compositor.
            Some(..) => (*transform, *subpixel_offset),
            None => {
                let origin = stacking_context.bounds.origin + *subpixel_offset;
                let pixel_snapped_origin =
                    Point2D::new(origin.x.to_nearest_pixel(pixels_per_px.get()),
                                 origin.y.to_nearest_pixel(pixels_per_px.get()));

                let transform = transform
                    .pre_translated(pixel_snapped_origin.x as AzFloat,
                                    pixel_snapped_origin.y as AzFloat,
                                    0.0)
                    .pre_mul(&stacking_context.transform);

                if transform.is_identity_or_simple_translation() {
                    let pixel_snapped_origin = Point2D::new(Au::from_f32_px(pixel_snapped_origin.x),
                                                            Au::from_f32_px(pixel_snapped_origin.y));
                    (transform, origin - pixel_snapped_origin)
                } else {
                    // In the case of a more complicated transformation, don't attempt to
                    // preserve subpixel offsets. This causes problems with reference tests
                    // that do scaling and rotation and it's unclear if we even want to be doing
                    // this.
                    (transform, Point2D::zero())
                }
            }
        };

        let transformed_transform =
            match transformed_tile_rect(paint_context.screen_rect, &transform) {
                Some(transformed) => transformed,
                None => {
                    // https://drafts.csswg.org/css-transforms/#transform-function-lists
                    // If a transform function causes the current transformation matrix (CTM)
                    // of an object to be non-invertible, the object and its content do not
                    // get displayed.
                    return;
                },
            };

        {
            let mut paint_subcontext = PaintContext {
                draw_target: draw_target.clone(),
                font_context: &mut *paint_context.font_context,
                page_rect: paint_context.page_rect,
                screen_rect: paint_context.screen_rect,
                clip_rect: Some(stacking_context.overflow),
                transient_clip: None,
                layer_kind: paint_context.layer_kind,
                subpixel_offset: subpixel_offset,
            };

            // Set up our clip rect and transform.
            paint_subcontext.draw_target.set_transform(&transform.to_2d());
            paint_subcontext.push_clip_if_applicable();

            self.draw_with_state(traversal,
                                 &mut paint_subcontext,
                                 &transform,
                                 &subpixel_offset,
                                 Some(transformed_transform));

            paint_subcontext.remove_transient_clip_if_applicable();
            paint_subcontext.pop_clip_if_applicable();
        }

        draw_target.set_transform(&old_transform);
        paint_context.draw_temporary_draw_target_if_necessary(
            &draw_target, &stacking_context.filters, stacking_context.blend_mode);
    }

    // Return all nodes containing the point of interest, bottommost first, and
    // respecting the `pointer-events` CSS property.
    pub fn hit_test(&self,
                    translated_point: &Point2D<Au>,
                    client_point: &Point2D<Au>,
                    scroll_offsets: &ScrollOffsetMap)
                    -> Vec<DisplayItemMetadata> {
        let mut result = Vec::new();
        let mut traversal = DisplayListTraversal::new(self);
        self.hit_test_contents(&mut traversal,
                               translated_point,
                               client_point,
                               scroll_offsets,
                               &mut result);
        result
    }

    pub fn hit_test_contents<'a>(&self,
                                 traversal: &mut DisplayListTraversal<'a>,
                                 translated_point: &Point2D<Au>,
                                 client_point: &Point2D<Au>,
                                 scroll_offsets: &ScrollOffsetMap,
                                 result: &mut Vec<DisplayItemMetadata>) {
        while let Some(item) = traversal.next() {
            match item {
                &DisplayItem::PushStackingContext(ref stacking_context_item) => {
                    self.hit_test_stacking_context(traversal,
                                                   &stacking_context_item.stacking_context,
                                                   translated_point,
                                                   client_point,
                                                   scroll_offsets,
                                                   result);
                }
                &DisplayItem::PopStackingContext(_) => return,
                _ => {
                    if let Some(meta) = item.hit_test(*translated_point) {
                        result.push(meta);
                    }
                }
            }
        }
    }

    fn hit_test_stacking_context<'a>(&self,
                        traversal: &mut DisplayListTraversal<'a>,
                        stacking_context: &StackingContext,
                        translated_point: &Point2D<Au>,
                        client_point: &Point2D<Au>,
                        scroll_offsets: &ScrollOffsetMap,
                        result: &mut Vec<DisplayItemMetadata>) {
        let is_fixed = stacking_context.layer_info.map_or(false,
            |info| info.scroll_policy == ScrollPolicy::FixedPosition);

        // Convert the parent translated point into stacking context local transform space if the
        // stacking context isn't fixed.  If it's fixed, we need to use the client point anyway.
        debug_assert!(stacking_context.context_type == StackingContextType::Real);
        let mut translated_point = if is_fixed {
            *client_point
        } else {
            let point = *translated_point - stacking_context.bounds.origin;
            let inv_transform = stacking_context.transform.inverse().unwrap();
            let frac_point = inv_transform.transform_point(&Point2D::new(point.x.to_f32_px(),
                                                                         point.y.to_f32_px()));
            Point2D::new(Au::from_f32_px(frac_point.x), Au::from_f32_px(frac_point.y))
        };

        // Adjust the translated point to account for the scroll offset if
        // necessary. This can only happen when WebRender is in use.
        //
        // We don't perform this adjustment on the root stacking context because
        // the DOM-side code has already translated the point for us (e.g. in
        // `Window::hit_test_query()`) by now.
        if !is_fixed && stacking_context.id != StackingContextId::root() {
            if let Some(scroll_offset) = scroll_offsets.get(&stacking_context.id) {
                translated_point.x -= Au::from_f32_px(scroll_offset.x);
                translated_point.y -= Au::from_f32_px(scroll_offset.y);
            }
        }

        self.hit_test_contents(traversal, &translated_point, client_point, scroll_offsets, result);
    }

    pub fn print(&self) {
        let mut print_tree = PrintTree::new("Display List".to_owned());
        self.print_with_tree(&mut print_tree);
    }

    pub fn print_with_tree(&self, print_tree: &mut PrintTree) {
        print_tree.new_level("Items".to_owned());
        for item in &self.list {
            print_tree.add_item(format!("{:?} StackingContext: {:?}",
                                        item,
                                        item.base().stacking_context_id));
        }
        print_tree.end_level();
    }
}

pub struct DisplayListTraversal<'a> {
    pub display_list: &'a DisplayList,
    pub next_item_index: usize,
    pub first_item_index: usize,
    pub last_item_index: usize,
}

impl<'a> DisplayListTraversal<'a> {
    pub fn new(display_list: &'a DisplayList) -> DisplayListTraversal {
        DisplayListTraversal {
            display_list: display_list,
            next_item_index: 0,
            first_item_index: 0,
            last_item_index: display_list.list.len(),
        }
    }

    pub fn new_partial(display_list: &'a DisplayList,
                       stacking_context_id: StackingContextId,
                       start: usize,
                       end: usize)
                       -> DisplayListTraversal {
        debug_assert!(start <= end);
        debug_assert!(display_list.list.len() > start);
        debug_assert!(display_list.list.len() > end);

        let stacking_context_start = display_list.list[0..start].iter().rposition(|item|
            match item {
                &DisplayItem::PushStackingContext(ref item) =>
                    item.stacking_context.id == stacking_context_id,
                _ => false,
            }).unwrap_or(start);
        debug_assert!(stacking_context_start <= start);

        DisplayListTraversal {
            display_list: display_list,
            next_item_index: stacking_context_start,
            first_item_index: start,
            last_item_index: end + 1,
        }
    }

    pub fn previous_item_id(&self) -> usize {
        self.next_item_index - 1
    }

    pub fn skip_to_end_of_stacking_context(&mut self, id: StackingContextId) {
        self.next_item_index = self.display_list.list[self.next_item_index..].iter()
                                                                             .position(|item| {
            match item {
                &DisplayItem::PopStackingContext(ref item) => item.stacking_context_id == id,
                _ => false
            }
        }).unwrap_or(self.display_list.list.len());
        debug_assert!(self.next_item_index < self.last_item_index);
    }
}

impl<'a> Iterator for DisplayListTraversal<'a> {
    type Item = &'a DisplayItem;

    fn next(&mut self) -> Option<&'a DisplayItem> {
        while self.next_item_index < self.last_item_index {
            debug_assert!(self.next_item_index <= self.last_item_index);

            let reached_first_item = self.next_item_index >= self.first_item_index;
            let item = &self.display_list.list[self.next_item_index];

            self.next_item_index += 1;

            if reached_first_item {
                return Some(item)
            }

            // Before we reach the starting item, we only emit stacking context boundaries. This
            // is to ensure that we properly position items when we are processing a display list
            // slice that is relative to a certain stacking context.
            match item {
                &DisplayItem::PushStackingContext(_) |
                &DisplayItem::PopStackingContext(_) => return Some(item),
                _ => {}
            }
        }

        None
    }
}

fn transformed_tile_rect(tile_rect: TypedRect<usize, ScreenPx>,
                         transform: &Matrix4D<f32>)
                         -> Option<Rect<Au>> {
    // Invert the current transform, then use this to back transform
    // the tile rect (placed at the origin) into the space of this
    // stacking context.
    let inverse_transform = match transform.inverse() {
        Some(inverse) => inverse,
        None => return None,
    };
    let inverse_transform_2d = inverse_transform.to_2d();
    let tile_size = Size2D::new(tile_rect.to_f32().size.width, tile_rect.to_f32().size.height);
    let tile_rect = Rect::new(Point2D::zero(), tile_size).to_untyped();
    Some(geometry::f32_rect_to_au_rect(inverse_transform_2d.transform_rect(&tile_rect)))
}


/// Display list sections that make up a stacking context. Each section  here refers
/// to the steps in CSS 2.1 Appendix E.
///
#[derive(Clone, Copy, Debug, Deserialize, Eq, HeapSizeOf, Ord, PartialEq, PartialOrd, RustcEncodable, Serialize)]
pub enum DisplayListSection {
    BackgroundAndBorders,
    BlockBackgroundsAndBorders,
    Content,
    Outlines,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, HeapSizeOf, Ord, PartialEq, PartialOrd, RustcEncodable, Serialize)]
pub enum StackingContextType {
    Real,
    PseudoPositioned,
    PseudoFloat,
}

#[derive(Clone, HeapSizeOf, Deserialize, Serialize)]
/// Represents one CSS stacking context, which may or may not have a hardware layer.
pub struct StackingContext {
    /// The ID of this StackingContext for uniquely identifying it.
    pub id: StackingContextId,

    /// The type of this StackingContext. Used for collecting and sorting.
    pub context_type: StackingContextType,

    /// The position and size of this stacking context.
    pub bounds: Rect<Au>,

    /// The overflow rect for this stacking context in its coordinate system.
    pub overflow: Rect<Au>,

    /// The `z-index` for this stacking context.
    pub z_index: i32,

    /// CSS filters to be applied to this stacking context (including opacity).
    pub filters: filter::T,

    /// The blend mode with which this stacking context blends with its backdrop.
    pub blend_mode: mix_blend_mode::T,

    /// A transform to be applied to this stacking context.
    pub transform: Matrix4D<f32>,

    /// The perspective matrix to be applied to children.
    pub perspective: Matrix4D<f32>,

    /// Whether this stacking context creates a new 3d rendering context.
    pub establishes_3d_context: bool,

    /// The layer info for this stacking context, if there is any.
    pub layer_info: Option<LayerInfo>,

    /// Children of this StackingContext.
    pub children: Vec<StackingContext>,

    /// If this StackingContext scrolls its overflow area, this will contain the id.
    pub overflow_scroll_id: Option<StackingContextId>,
}

impl StackingContext {
    /// Creates a new stacking context.
    #[inline]
    pub fn new(id: StackingContextId,
               context_type: StackingContextType,
               bounds: &Rect<Au>,
               overflow: &Rect<Au>,
               z_index: i32,
               filters: filter::T,
               blend_mode: mix_blend_mode::T,
               transform: Matrix4D<f32>,
               perspective: Matrix4D<f32>,
               establishes_3d_context: bool,
               layer_info: Option<LayerInfo>,
               scroll_id: Option<StackingContextId>)
               -> StackingContext {
        StackingContext {
            id: id,
            context_type: context_type,
            bounds: *bounds,
            overflow: *overflow,
            z_index: z_index,
            filters: filters,
            blend_mode: blend_mode,
            transform: transform,
            perspective: perspective,
            establishes_3d_context: establishes_3d_context,
            layer_info: layer_info,
            children: Vec::new(),
            overflow_scroll_id: scroll_id,
        }
    }

    pub fn add_child(&mut self, mut child: StackingContext) {
        child.update_overflow_for_all_children();
        self.children.push(child);
    }

    pub fn child_at_mut(&mut self, index: usize) -> &mut StackingContext {
        &mut self.children[index]
    }

    pub fn children(&self) -> &[StackingContext] {
        &self.children
    }

    fn update_overflow_for_all_children(&mut self) {
        for child in self.children.iter() {
            if self.context_type == StackingContextType::Real &&
               child.context_type == StackingContextType::Real {
                // This child might be transformed, so we need to take into account
                // its transformed overflow rect too, but at the correct position.
                let overflow = child.overflow_rect_in_parent_space();
                self.overflow = self.overflow.union(&overflow);
            }
        }
    }

    fn overflow_rect_in_parent_space(&self) -> Rect<Au> {
        // Transform this stacking context to get it into the same space as
        // the parent stacking context.
        //
        // TODO: Take into account 3d transforms, even though it's a fairly
        // uncommon case.
        let origin_x = self.bounds.origin.x.to_f32_px();
        let origin_y = self.bounds.origin.y.to_f32_px();

        let transform = Matrix4D::identity().pre_translated(origin_x, origin_y, 0.0)
                                            .pre_mul(&self.transform);
        let transform_2d = transform.to_2d();

        let overflow = geometry::au_rect_to_f32_rect(self.overflow);
        let overflow = transform_2d.transform_rect(&overflow);
        geometry::f32_rect_to_au_rect(overflow)
    }

    pub fn print_with_tree(&self, print_tree: &mut PrintTree) {
        print_tree.new_level(format!("{:?}", self));
        for kid in self.children() {
            kid.print_with_tree(print_tree);
        }
        print_tree.end_level();
    }

    fn intersects_rect_in_parent_context(&self, rect: Option<Rect<Au>>) -> bool {
        // We only do intersection checks for real stacking contexts, since
        // pseudo stacking contexts might not have proper position information.
        if self.context_type != StackingContextType::Real {
            return true;
        }

        let rect = match rect {
            Some(ref rect) => rect,
            None => return true,
        };

        self.overflow_rect_in_parent_space().intersects(rect)
    }
}

impl Ord for StackingContext {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.z_index != 0 || other.z_index != 0 {
            return self.z_index.cmp(&other.z_index);
        }

        match (self.context_type, other.context_type) {
            (StackingContextType::PseudoFloat, StackingContextType::PseudoFloat) => Ordering::Equal,
            (StackingContextType::PseudoFloat, _) => Ordering::Less,
            (_, StackingContextType::PseudoFloat) => Ordering::Greater,
            (_, _) => Ordering::Equal,
        }
    }
}

impl PartialOrd for StackingContext {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for StackingContext {}
impl PartialEq for StackingContext {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl fmt::Debug for StackingContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let type_string = if self.layer_info.is_some() {
            "Layered StackingContext"
        } else if self.context_type == StackingContextType::Real {
            "StackingContext"
        } else {
            "Pseudo-StackingContext"
        };

        let scrollable_string = if self.overflow_scroll_id.is_some() {
            " (scrolls overflow area)"
        } else {
            ""
        };

        write!(f, "{}{} at {:?} with overflow {:?}: {:?}",
               type_string,
               scrollable_string,
               self.bounds,
               self.overflow,
               self.id)
    }
}

/// One drawing command in the list.
#[derive(Clone, Deserialize, HeapSizeOf, Serialize)]
pub enum DisplayItem {
    SolidColor(Box<SolidColorDisplayItem>),
    Text(Box<TextDisplayItem>),
    Image(Box<ImageDisplayItem>),
    WebGL(Box<WebGLDisplayItem>),
    Border(Box<BorderDisplayItem>),
    Gradient(Box<GradientDisplayItem>),
    Line(Box<LineDisplayItem>),
    BoxShadow(Box<BoxShadowDisplayItem>),
    Iframe(Box<IframeDisplayItem>),
    PushStackingContext(Box<PushStackingContextItem>),
    PopStackingContext(Box<PopStackingContextItem>),
}

/// Information common to all display items.
#[derive(Clone, Deserialize, HeapSizeOf, Serialize)]
pub struct BaseDisplayItem {
    /// The boundaries of the display item, in layer coordinates.
    pub bounds: Rect<Au>,

    /// Metadata attached to this display item.
    pub metadata: DisplayItemMetadata,

    /// The region to clip to.
    pub clip: ClippingRegion,

    /// The section of the display list that this item belongs to.
    pub section: DisplayListSection,

    /// The id of the stacking context this item belongs to.
    pub stacking_context_id: StackingContextId,
}

impl BaseDisplayItem {
    #[inline(always)]
    pub fn new(bounds: &Rect<Au>,
               metadata: DisplayItemMetadata,
               clip: &ClippingRegion,
               section: DisplayListSection,
               stacking_context_id: StackingContextId)
               -> BaseDisplayItem {
        // Detect useless clipping regions here and optimize them to `ClippingRegion::max()`.
        // The painting backend may want to optimize out clipping regions and this makes it easier
        // for it to do so.
        BaseDisplayItem {
            bounds: *bounds,
            metadata: metadata,
            clip: if clip.does_not_clip_rect(&bounds) {
                ClippingRegion::max()
            } else {
                (*clip).clone()
            },
            section: section,
            stacking_context_id: stacking_context_id,
        }
    }

    #[inline(always)]
    pub fn empty() -> BaseDisplayItem {
        BaseDisplayItem {
            bounds: TypedRect::zero(),
            metadata: DisplayItemMetadata {
                node: OpaqueNode(0),
                pointing: None,
            },
            clip: ClippingRegion::max(),
            section: DisplayListSection::Content,
            stacking_context_id: StackingContextId::root(),
        }
    }
}

/// A clipping region for a display item. Currently, this can describe rectangles, rounded
/// rectangles (for `border-radius`), or arbitrary intersections of the two. Arbitrary transforms
/// are not supported because those are handled by the higher-level `StackingContext` abstraction.
#[derive(Clone, PartialEq, HeapSizeOf, Deserialize, Serialize)]
pub struct ClippingRegion {
    /// The main rectangular region. This does not include any corners.
    pub main: Rect<Au>,
    /// Any complex regions.
    ///
    /// TODO(pcwalton): Atomically reference count these? Not sure if it's worth the trouble.
    /// Measure and follow up.
    pub complex: Vec<ComplexClippingRegion>,
}

/// A complex clipping region. These don't as easily admit arbitrary intersection operations, so
/// they're stored in a list over to the side. Currently a complex clipping region is just a
/// rounded rectangle, but the CSS WGs will probably make us throw more stuff in here eventually.
#[derive(Clone, PartialEq, Debug, HeapSizeOf, Deserialize, Serialize)]
pub struct ComplexClippingRegion {
    /// The boundaries of the rectangle.
    pub rect: Rect<Au>,
    /// Border radii of this rectangle.
    pub radii: BorderRadii<Au>,
}

impl ClippingRegion {
    /// Returns an empty clipping region that, if set, will result in no pixels being visible.
    #[inline]
    pub fn empty() -> ClippingRegion {
        ClippingRegion {
            main: Rect::zero(),
            complex: Vec::new(),
        }
    }

    /// Returns an all-encompassing clipping region that clips no pixels out.
    #[inline]
    pub fn max() -> ClippingRegion {
        ClippingRegion {
            main: max_rect(),
            complex: Vec::new(),
        }
    }

    /// Returns a clipping region that represents the given rectangle.
    #[inline]
    pub fn from_rect(rect: &Rect<Au>) -> ClippingRegion {
        ClippingRegion {
            main: *rect,
            complex: Vec::new(),
        }
    }

    /// Mutates this clipping region to intersect with the given rectangle.
    ///
    /// TODO(pcwalton): This could more eagerly eliminate complex clipping regions, at the cost of
    /// complexity.
    #[inline]
    pub fn intersect_rect(&mut self, rect: &Rect<Au>) {
        self.main = self.main.intersection(rect).unwrap_or(Rect::zero())
    }

    /// Returns true if this clipping region might be nonempty. This can return false positives,
    /// but never false negatives.
    #[inline]
    pub fn might_be_nonempty(&self) -> bool {
        !self.main.is_empty()
    }

    /// Returns true if this clipping region might contain the given point and false otherwise.
    /// This is a quick, not a precise, test; it can yield false positives.
    #[inline]
    pub fn might_intersect_point(&self, point: &Point2D<Au>) -> bool {
        self.main.contains(point) &&
            self.complex.iter().all(|complex| complex.rect.contains(point))
    }

    /// Returns true if this clipping region might intersect the given rectangle and false
    /// otherwise. This is a quick, not a precise, test; it can yield false positives.
    #[inline]
    pub fn might_intersect_rect(&self, rect: &Rect<Au>) -> bool {
        self.main.intersects(rect) &&
            self.complex.iter().all(|complex| complex.rect.intersects(rect))
    }

    /// Returns true if this clipping region completely surrounds the given rect.
    #[inline]
    pub fn does_not_clip_rect(&self, rect: &Rect<Au>) -> bool {
        self.main.contains(&rect.origin) && self.main.contains(&rect.bottom_right()) &&
            self.complex.iter().all(|complex| {
                complex.rect.contains(&rect.origin) && complex.rect.contains(&rect.bottom_right())
            })
    }

    /// Returns a bounding rect that surrounds this entire clipping region.
    #[inline]
    pub fn bounding_rect(&self) -> Rect<Au> {
        let mut rect = self.main;
        for complex in &*self.complex {
            rect = rect.union(&complex.rect)
        }
        rect
    }

    /// Intersects this clipping region with the given rounded rectangle.
    #[inline]
    pub fn intersect_with_rounded_rect(&mut self, rect: &Rect<Au>, radii: &BorderRadii<Au>) {
        let new_complex_region = ComplexClippingRegion {
            rect: *rect,
            radii: *radii,
        };

        // FIXME(pcwalton): This is O(n²) worst case for disjoint clipping regions. Is that OK?
        // They're slow anyway…
        //
        // Possibly relevant if we want to do better:
        //
        //     http://www.inrg.csie.ntu.edu.tw/algorithm2014/presentation/D&C%20Lee-84.pdf
        for existing_complex_region in &mut self.complex {
            if existing_complex_region.completely_encloses(&new_complex_region) {
                *existing_complex_region = new_complex_region;
                return
            }
            if new_complex_region.completely_encloses(existing_complex_region) {
                return
            }
        }

        self.complex.push(ComplexClippingRegion {
            rect: *rect,
            radii: *radii,
        });
    }

    /// Translates this clipping region by the given vector.
    #[inline]
    pub fn translate(&self, delta: &Point2D<Au>) -> ClippingRegion {
        ClippingRegion {
            main: self.main.translate(delta),
            complex: self.complex.iter().map(|complex| {
                ComplexClippingRegion {
                    rect: complex.rect.translate(delta),
                    radii: complex.radii,
                }
            }).collect(),
        }
    }

    #[inline]
    pub fn is_max(&self) -> bool {
        self.main == max_rect() && self.complex.is_empty()
    }
}

impl fmt::Debug for ClippingRegion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if *self == ClippingRegion::max() {
            write!(f, "ClippingRegion::Max")
        } else if *self == ClippingRegion::empty() {
            write!(f, "ClippingRegion::Empty")
        } else if self.main == max_rect() {
            write!(f, "ClippingRegion(Complex={:?})", self.complex)
        } else {
            write!(f, "ClippingRegion(Rect={:?}, Complex={:?})", self.main, self.complex)
        }
    }
}

impl ComplexClippingRegion {
    // TODO(pcwalton): This could be more aggressive by considering points that touch the inside of
    // the border radius ellipse.
    fn completely_encloses(&self, other: &ComplexClippingRegion) -> bool {
        let left = cmp::max(self.radii.top_left.width, self.radii.bottom_left.width);
        let top = cmp::max(self.radii.top_left.height, self.radii.top_right.height);
        let right = cmp::max(self.radii.top_right.width, self.radii.bottom_right.width);
        let bottom = cmp::max(self.radii.bottom_left.height, self.radii.bottom_right.height);
        let interior = Rect::new(Point2D::new(self.rect.origin.x + left, self.rect.origin.y + top),
                                 Size2D::new(self.rect.size.width - left - right,
                                             self.rect.size.height - top - bottom));
        interior.origin.x <= other.rect.origin.x && interior.origin.y <= other.rect.origin.y &&
            interior.max_x() >= other.rect.max_x() && interior.max_y() >= other.rect.max_y()
    }
}

/// Metadata attached to each display item. This is useful for performing auxiliary threads with
/// the display list involving hit testing: finding the originating DOM node and determining the
/// cursor to use when the element is hovered over.
#[derive(Clone, Copy, HeapSizeOf, Deserialize, Serialize)]
pub struct DisplayItemMetadata {
    /// The DOM node from which this display item originated.
    pub node: OpaqueNode,
    /// The value of the `cursor` property when the mouse hovers over this display item. If `None`,
    /// this display item is ineligible for pointer events (`pointer-events: none`).
    pub pointing: Option<Cursor>,
}

/// Paints a solid color.
#[derive(Clone, HeapSizeOf, Deserialize, Serialize)]
pub struct SolidColorDisplayItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    /// The color.
    pub color: Color,
}

/// Paints text.
#[derive(Clone, HeapSizeOf, Deserialize, Serialize)]
pub struct TextDisplayItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    /// The text run.
    #[ignore_heap_size_of = "Because it is non-owning"]
    pub text_run: Arc<TextRun>,

    /// The range of text within the text run.
    pub range: Range<ByteIndex>,

    /// The color of the text.
    pub text_color: Color,

    /// The position of the start of the baseline of this text.
    pub baseline_origin: Point2D<Au>,

    /// The orientation of the text: upright or sideways left/right.
    pub orientation: TextOrientation,

    /// The blur radius for this text. If zero, this text is not blurred.
    pub blur_radius: Au,
}

#[derive(Clone, Eq, PartialEq, HeapSizeOf, Deserialize, Serialize)]
pub enum TextOrientation {
    Upright,
    SidewaysLeft,
    SidewaysRight,
}

/// Paints an image.
#[derive(Clone, HeapSizeOf, Deserialize, Serialize)]
pub struct ImageDisplayItem {
    pub base: BaseDisplayItem,

    pub webrender_image: WebRenderImageInfo,

    #[ignore_heap_size_of = "Because it is non-owning"]
    pub image_data: Option<Arc<IpcSharedMemory>>,

    /// The dimensions to which the image display item should be stretched. If this is smaller than
    /// the bounds of this display item, then the image will be repeated in the appropriate
    /// direction to tile the entire bounds.
    pub stretch_size: Size2D<Au>,

    /// The amount of space to add to the right and bottom part of each tile, when the image
    /// is tiled.
    pub tile_spacing: Size2D<Au>,

    /// The algorithm we should use to stretch the image. See `image_rendering` in CSS-IMAGES-3 §
    /// 5.3.
    pub image_rendering: image_rendering::T,
}

#[derive(Clone, HeapSizeOf, Deserialize, Serialize)]
pub struct WebGLDisplayItem {
    pub base: BaseDisplayItem,
    #[ignore_heap_size_of = "Defined in webrender_traits"]
    pub context_id: WebGLContextId,
}


/// Paints an iframe.
#[derive(Clone, HeapSizeOf, Deserialize, Serialize)]
pub struct IframeDisplayItem {
    pub base: BaseDisplayItem,
    pub iframe: PipelineId,
}

/// Paints a gradient.
#[derive(Clone, Deserialize, HeapSizeOf, Serialize)]
pub struct GradientDisplayItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    /// The start point of the gradient (computed during display list construction).
    pub start_point: Point2D<Au>,

    /// The end point of the gradient (computed during display list construction).
    pub end_point: Point2D<Au>,

    /// A list of color stops.
    pub stops: Vec<GradientStop>,
}

/// Paints a border.
#[derive(Clone, HeapSizeOf, Deserialize, Serialize)]
pub struct BorderDisplayItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    /// Border widths.
    pub border_widths: SideOffsets2D<Au>,

    /// Border colors.
    pub color: SideOffsets2D<Color>,

    /// Border styles.
    pub style: SideOffsets2D<border_style::T>,

    /// Border radii.
    ///
    /// TODO(pcwalton): Elliptical radii.
    pub radius: BorderRadii<Au>,
}

/// Information about the border radii.
///
/// TODO(pcwalton): Elliptical radii.
#[derive(Clone, PartialEq, Debug, Copy, HeapSizeOf, Deserialize, Serialize)]
pub struct BorderRadii<T> {
    pub top_left: Size2D<T>,
    pub top_right: Size2D<T>,
    pub bottom_right: Size2D<T>,
    pub bottom_left: Size2D<T>,
}

impl<T> Default for BorderRadii<T> where T: Default, T: Clone {
    fn default() -> Self {
        let top_left = Size2D::new(Default::default(),
                                   Default::default());
        let top_right = Size2D::new(Default::default(),
                                    Default::default());
        let bottom_left = Size2D::new(Default::default(),
                                      Default::default());
        let bottom_right = Size2D::new(Default::default(),
                                       Default::default());
        BorderRadii { top_left: top_left,
                      top_right: top_right,
                      bottom_left: bottom_left,
                      bottom_right: bottom_right }
    }
}

impl BorderRadii<Au> {
    // Scale the border radii by the specified factor
    pub fn scale_by(&self, s: f32) -> BorderRadii<Au> {
        BorderRadii { top_left: BorderRadii::scale_corner_by(self.top_left, s),
                      top_right: BorderRadii::scale_corner_by(self.top_right, s),
                      bottom_left: BorderRadii::scale_corner_by(self.bottom_left, s),
                      bottom_right: BorderRadii::scale_corner_by(self.bottom_right, s) }
    }

    // Scale the border corner radius by the specified factor
    pub fn scale_corner_by(corner: Size2D<Au>, s: f32) -> Size2D<Au> {
        Size2D::new(corner.width.scale_by(s), corner.height.scale_by(s))
    }
}

impl<T> BorderRadii<T> where T: PartialEq + Zero {
    /// Returns true if all the radii are zero.
    pub fn is_square(&self) -> bool {
        let zero = Zero::zero();
        self.top_left == zero && self.top_right == zero && self.bottom_right == zero &&
            self.bottom_left == zero
    }
}

impl<T> BorderRadii<T> where T: PartialEq + Zero + Clone {
    /// Returns a set of border radii that all have the given value.
    pub fn all_same(value: T) -> BorderRadii<T> {
        BorderRadii {
            top_left: Size2D::new(value.clone(), value.clone()),
            top_right: Size2D::new(value.clone(), value.clone()),
            bottom_right: Size2D::new(value.clone(), value.clone()),
            bottom_left: Size2D::new(value.clone(), value.clone()),
        }
    }
}

/// Paints a line segment.
#[derive(Clone, HeapSizeOf, Deserialize, Serialize)]
pub struct LineDisplayItem {
    pub base: BaseDisplayItem,

    /// The line segment color.
    pub color: Color,

    /// The line segment style.
    pub style: border_style::T
}

/// Paints a box shadow per CSS-BACKGROUNDS.
#[derive(Clone, HeapSizeOf, Deserialize, Serialize)]
pub struct BoxShadowDisplayItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    /// The dimensions of the box that we're placing a shadow around.
    pub box_bounds: Rect<Au>,

    /// The offset of this shadow from the box.
    pub offset: Point2D<Au>,

    /// The color of this shadow.
    pub color: Color,

    /// The blur radius for this shadow.
    pub blur_radius: Au,

    /// The spread radius of this shadow.
    pub spread_radius: Au,

    /// The border radius of this shadow.
    ///
    /// TODO(pcwalton): Elliptical radii; different radii for each corner.
    pub border_radius: Au,

    /// How we should clip the result.
    pub clip_mode: BoxShadowClipMode,
}

/// Defines a stacking context.
#[derive(Clone, HeapSizeOf, Deserialize, Serialize)]
pub struct PushStackingContextItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    pub stacking_context: StackingContext,
}

/// Defines a stacking context.
#[derive(Clone, HeapSizeOf, Deserialize, Serialize)]
pub struct PopStackingContextItem {
    /// Fields common to all display items.
    pub base: BaseDisplayItem,

    pub stacking_context_id: StackingContextId,
}


/// How a box shadow should be clipped.
#[derive(Clone, Copy, Debug, PartialEq, HeapSizeOf, Deserialize, Serialize)]
pub enum BoxShadowClipMode {
    /// No special clipping should occur. This is used for (shadowed) text decorations.
    None,
    /// The area inside `box_bounds` should be clipped out. Corresponds to the normal CSS
    /// `box-shadow`.
    Outset,
    /// The area outside `box_bounds` should be clipped out. Corresponds to the `inset` flag on CSS
    /// `box-shadow`.
    Inset,
}

impl DisplayItem {
    /// Paints this display item into the given painting context.
    fn draw_into_context(&self, paint_context: &mut PaintContext) {
        let this_clip = &self.base().clip;
        match paint_context.transient_clip {
            Some(ref transient_clip) if transient_clip == this_clip => {}
            Some(_) | None => paint_context.push_transient_clip((*this_clip).clone()),
        }

        match *self {
            DisplayItem::SolidColor(ref solid_color) => {
                if !solid_color.color.a.approx_eq(&0.0) {
                    paint_context.draw_solid_color(&solid_color.base.bounds, solid_color.color)
                }
            }

            DisplayItem::Text(ref text) => {
                debug!("Drawing text at {:?}.", text.base.bounds);
                paint_context.draw_text(&**text);
            }

            DisplayItem::Image(ref image_item) => {
                debug!("Drawing image at {:?}.", image_item.base.bounds);
                paint_context.draw_image(
                    &image_item.base.bounds,
                    &image_item.stretch_size,
                    &image_item.tile_spacing,
                    &image_item.webrender_image,
                    &image_item.image_data
                               .as_ref()
                               .expect("Non-WR painting needs image data!")[..],
                    image_item.image_rendering.clone());
            }

            DisplayItem::WebGL(_) => {
                panic!("Shouldn't be here, WebGL display items are created just with webrender");
            }

            DisplayItem::Border(ref border) => {
                paint_context.draw_border(&border.base.bounds,
                                          &border.border_widths,
                                          &border.radius,
                                          &border.color,
                                          &border.style)
            }

            DisplayItem::Gradient(ref gradient) => {
                paint_context.draw_linear_gradient(&gradient.base.bounds,
                                                   &gradient.start_point,
                                                   &gradient.end_point,
                                                   &gradient.stops);
            }

            DisplayItem::Line(ref line) => {
                paint_context.draw_line(&line.base.bounds, line.color, line.style)
            }

            DisplayItem::BoxShadow(ref box_shadow) => {
                paint_context.draw_box_shadow(&box_shadow.box_bounds,
                                              &box_shadow.offset,
                                              box_shadow.color,
                                              box_shadow.blur_radius,
                                              box_shadow.spread_radius,
                                              box_shadow.clip_mode);
            }

            DisplayItem::Iframe(..) => {}

            DisplayItem::PushStackingContext(..) => {}

            DisplayItem::PopStackingContext(..) => {}
        }
    }

    pub fn intersects_rect_in_parent_context(&self, rect: Option<Rect<Au>>) -> bool {
        let rect = match rect {
            Some(ref rect) => rect,
            None => return true,
        };

        if !rect.intersects(&self.bounds()) {
            return false;
        }

        self.base().clip.might_intersect_rect(&rect)
    }

    pub fn base(&self) -> &BaseDisplayItem {
        match *self {
            DisplayItem::SolidColor(ref solid_color) => &solid_color.base,
            DisplayItem::Text(ref text) => &text.base,
            DisplayItem::Image(ref image_item) => &image_item.base,
            DisplayItem::WebGL(ref webgl_item) => &webgl_item.base,
            DisplayItem::Border(ref border) => &border.base,
            DisplayItem::Gradient(ref gradient) => &gradient.base,
            DisplayItem::Line(ref line) => &line.base,
            DisplayItem::BoxShadow(ref box_shadow) => &box_shadow.base,
            DisplayItem::Iframe(ref iframe) => &iframe.base,
            DisplayItem::PushStackingContext(ref stacking_context) => &stacking_context.base,
            DisplayItem::PopStackingContext(ref item) => &item.base,
        }
    }

    pub fn stacking_context_id(&self) -> StackingContextId {
        self.base().stacking_context_id
    }

    pub fn section(&self) -> DisplayListSection {
        self.base().section
    }

    pub fn bounds(&self) -> Rect<Au> {
        self.base().bounds
    }

    pub fn debug_with_level(&self, level: u32) {
        let mut indent = String::new();
        for _ in 0..level {
            indent.push_str("| ")
        }
        println!("{}+ {:?}", indent, self);
    }

    fn hit_test(&self, point: Point2D<Au>) -> Option<DisplayItemMetadata> {
        // TODO(pcwalton): Use a precise algorithm here. This will allow us to properly hit
        // test elements with `border-radius`, for example.
        let base_item = self.base();

        if !base_item.clip.might_intersect_point(&point) {
            // Clipped out.
            return None;
        }
        if !self.bounds().contains(&point) {
            // Can't possibly hit.
            return None;
        }
        if base_item.metadata.pointing.is_none() {
            // `pointer-events` is `none`. Ignore this item.
            return None;
        }

        match *self {
            DisplayItem::Border(ref border) => {
                // If the point is inside the border, it didn't hit the border!
                let interior_rect =
                    Rect::new(
                        Point2D::new(border.base.bounds.origin.x +
                                     border.border_widths.left,
                                     border.base.bounds.origin.y +
                                     border.border_widths.top),
                        Size2D::new(border.base.bounds.size.width -
                                    (border.border_widths.left +
                                     border.border_widths.right),
                                    border.base.bounds.size.height -
                                    (border.border_widths.top +
                                     border.border_widths.bottom)));
                if interior_rect.contains(&point) {
                    return None;
                }
            }
            DisplayItem::BoxShadow(_) => {
                // Box shadows can never be hit.
                return None;
            }
            _ => {}
        }

        Some(base_item.metadata)
    }
}

impl fmt::Debug for DisplayItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let DisplayItem::PushStackingContext(ref item) = *self {
            return write!(f, "PushStackingContext({:?})", item.stacking_context);
        }

        if let DisplayItem::PopStackingContext(ref item) = *self {
            return write!(f, "PopStackingContext({:?}", item.stacking_context_id);
        }

        write!(f, "{} @ {:?} {:?}",
            match *self {
                DisplayItem::SolidColor(ref solid_color) =>
                    format!("SolidColor rgba({}, {}, {}, {})",
                            solid_color.color.r,
                            solid_color.color.g,
                            solid_color.color.b,
                            solid_color.color.a),
                DisplayItem::Text(_) => "Text".to_owned(),
                DisplayItem::Image(_) => "Image".to_owned(),
                DisplayItem::WebGL(_) => "WebGL".to_owned(),
                DisplayItem::Border(_) => "Border".to_owned(),
                DisplayItem::Gradient(_) => "Gradient".to_owned(),
                DisplayItem::Line(_) => "Line".to_owned(),
                DisplayItem::BoxShadow(_) => "BoxShadow".to_owned(),
                DisplayItem::Iframe(_) => "Iframe".to_owned(),
                DisplayItem::PushStackingContext(_) => "".to_owned(),
                DisplayItem::PopStackingContext(_) => "".to_owned(),
            },
            self.bounds(),
            self.base().clip
        )
    }
}

#[derive(Copy, Clone, HeapSizeOf, Deserialize, Serialize)]
pub struct WebRenderImageInfo {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    #[ignore_heap_size_of = "WebRender traits type, and tiny"]
    pub key: Option<webrender_traits::ImageKey>,
}

impl WebRenderImageInfo {
    #[inline]
    pub fn from_image(image: &Image) -> WebRenderImageInfo {
        WebRenderImageInfo {
            width: image.width,
            height: image.height,
            format: image.format,
            key: image.id,
        }
    }
}

/// The type of the scroll offset list. This is only populated if WebRender is in use.
pub type ScrollOffsetMap = HashMap<StackingContextId, Point2D<f32>>;


pub trait SimpleMatrixDetection {
    fn is_identity_or_simple_translation(&self) -> bool;
}

impl SimpleMatrixDetection for Matrix4D<f32> {
    #[inline]
    fn is_identity_or_simple_translation(&self) -> bool {
        let (_0, _1) = (Zero::zero(), One::one());
        self.m11 == _1 && self.m12 == _0 && self.m13 == _0 && self.m14 == _0 &&
        self.m21 == _0 && self.m22 == _1 && self.m23 == _0 && self.m24 == _0 &&
        self.m31 == _0 && self.m32 == _0 && self.m33 == _1 && self.m34 == _0 &&
        self.m44 == _1
    }
}
