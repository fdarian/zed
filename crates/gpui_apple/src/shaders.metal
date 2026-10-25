#include <metal_stdlib>
#include <simd/simd.h>

using namespace metal;

float4 hsla_to_rgba(Hsla hsla);
float3 srgb_to_linear(float3 color);
float3 linear_to_srgb(float3 color);
float4 srgb_to_oklab(float4 color);
float4 oklab_to_srgb(float4 color);
float4 to_device_position(float2 unit_vertex, Bounds_ScaledPixels bounds,
                          constant Size_DevicePixels *viewport_size);
float4 to_device_position_transformed(float2 unit_vertex, Bounds_ScaledPixels bounds,
                          TransformationMatrix transformation,
                          constant Size_DevicePixels *input_viewport_size);

float2 to_tile_position(float2 unit_vertex, AtlasTile tile,
                        constant Size_DevicePixels *atlas_size);
float4 distance_from_clip_rect(float2 unit_vertex, Bounds_ScaledPixels bounds,
                               Bounds_ScaledPixels clip_bounds);
float4 distance_from_clip_rect_transformed(float2 unit_vertex, Bounds_ScaledPixels bounds,
                               Bounds_ScaledPixels clip_bounds, TransformationMatrix transformation);
float corner_dash_velocity(float dv1, float dv2);
float dash_alpha(float t, float period, float length, float dash_velocity,
                 float antialias_threshold, float pixel);
float dot_alpha(float t, float period, float dot_length, float dash_velocity,
                float across, float diameter, float antialias_threshold, float pixel);
float quarter_ellipse_sdf(float2 point, float2 radii);
float ellipse_corner_sdf(float2 corner_center_to_point, float2 radii);
float2 corner_radius_pair(float horizontal, float vertical);
float2 pick_corner_radius(float2 center_to_point, Corners_ScaledPixels corner_radii,
                          Corners_ScaledPixels corner_radii_y);
float quad_sdf(float2 point, Bounds_ScaledPixels bounds,
               Corners_ScaledPixels corner_radii,
               Corners_ScaledPixels corner_radii_y);
float quad_sdf_impl(float2 center_to_point, float2 corner_radius);
float content_mask_coverage(float2 position, ContentMask_ScaledPixels content_mask);
float border_vertical_weight(float2 corner_to_point, float2 border, float pixel);
bool is_identity_transform(TransformationMatrix transformation);
float transform_pixel_scale(TransformationMatrix transformation);
float2 apply_transform(TransformationMatrix transformation, float2 position);
float4 device_position_of(float2 position, constant Size_DevicePixels *viewport_size);
float4 distance_from_clip(float2 position, Bounds_ScaledPixels clip_bounds);
float local_clip_coverage(float2 local_position, ContentMask_ScaledPixels local_clip, float pixel);
float rect_coverage(float2 local_position, Bounds_ScaledPixels bounds, float pixel);
float4 select_border_color(bool is_left, bool is_top, float vertical_weight,
                           float4 top, float4 right, float4 bottom,
                           float4 left);
float gaussian(float x, float sigma);
float2 erf(float2 x);
float blur_along_x(float x, float y, float sigma, float2 corner,
                   float2 half_size);
float4 over(float4 below, float4 above);
float radians(float degrees);
float4 fill_color(Background background, float2 position, Bounds_ScaledPixels bounds,
  float4 solid_color, float4 color0, float4 color1);
float4 css_gradient_color(Background background, float2 position, Bounds_ScaledPixels bounds);
float4 dither(float4 color, float2 position);

// A vertex of a primitive that may be drawn through a matrix, see `transformed_vertex`.
struct TransformedVertex {
  float4 device_position;
  float4 clip_distance;
  // Across the primitive's bounds, 0 to 1 (a little beyond for the padding).
  float2 unit_position;
  // Where the vertex is in the primitive's own space; unused when `local_scale` is 0.
  float2 local_position;
  // The device pixels one unit of the primitive's own space covers, or 0 when no
  // matrix applies and the fragment works in window pixels.
  float local_scale;
};

TransformedVertex transformed_vertex(float2 unit_vertex, Bounds_ScaledPixels bounds,
                                     TransformationMatrix transformation,
                                     Bounds_ScaledPixels clip_bounds,
                                     constant Size_DevicePixels *viewport_size);

struct GradientColor {
  float4 solid;
  float4 color0;
  float4 color1;
};
GradientColor prepare_fill_color(uint tag, uint color_space, Hsla solid, Hsla color0, Hsla color1);

struct QuadVertexOutput {
  uint quad_id [[flat]];
  float4 position [[position]];
  float4 border_color_top [[flat]];
  float4 border_color_right [[flat]];
  float4 border_color_bottom [[flat]];
  float4 border_color_left [[flat]];
  float4 background_solid [[flat]];
  float4 background_color0 [[flat]];
  float4 background_color1 [[flat]];
  // Where the fragment is in the quad's own space; unused (and `local_scale` 0)
  // for a quad with no transformation.
  float2 local_position;
  float local_scale [[flat]];
  float clip_distance [[clip_distance]][4];
};

struct QuadFragmentInput {
  uint quad_id [[flat]];
  float4 position [[position]];
  float4 border_color_top [[flat]];
  float4 border_color_right [[flat]];
  float4 border_color_bottom [[flat]];
  float4 border_color_left [[flat]];
  float4 background_solid [[flat]];
  float4 background_color0 [[flat]];
  float4 background_color1 [[flat]];
  float2 local_position;
  float local_scale [[flat]];
};

vertex QuadVertexOutput quad_vertex(uint unit_vertex_id [[vertex_id]],
                                    uint quad_id [[instance_id]],
                                    constant float2 *unit_vertices
                                    [[buffer(QuadInputIndex_Vertices)]],
                                    constant Quad *quads
                                    [[buffer(QuadInputIndex_Quads)]],
                                    constant Size_DevicePixels *viewport_size
                                    [[buffer(QuadInputIndex_ViewportSize)]]) {
  float2 unit_vertex = unit_vertices[unit_vertex_id];
  Quad quad = quads[quad_id];
  float4 device_position;
  float4 clip_distance;
  float2 local_position = float2(0.0);
  float local_scale = 0.0;
  if (is_identity_transform(quad.transformation)) {
    device_position = to_device_position(unit_vertex, quad.bounds, viewport_size);
    clip_distance = distance_from_clip_rect(unit_vertex, quad.bounds,
                                            quad.content_mask.bounds);
  } else {
    // One device pixel of padding around the quad is where its antialiased
    // edges are drawn, whichever way the matrix turns them.
    local_scale = transform_pixel_scale(quad.transformation);
    float pad = 1.0 / local_scale;
    float2 origin = float2(quad.bounds.origin.x, quad.bounds.origin.y) - pad;
    float2 extent = float2(quad.bounds.size.width, quad.bounds.size.height) + 2.0 * pad;
    local_position = origin + unit_vertex * extent;
    float2 window_position = apply_transform(quad.transformation, local_position);
    device_position = device_position_of(window_position, viewport_size);
    clip_distance = distance_from_clip(window_position, quad.content_mask.bounds);
  }

  GradientColor gradient = prepare_fill_color(
    quad.background.tag,
    quad.background.color_space,
    quad.background.solid,
    quad.background.colors[0].color,
    quad.background.colors[1].color
  );

  return QuadVertexOutput{
      quad_id,
      device_position,
      hsla_to_rgba(quad.border_colors.top),
      hsla_to_rgba(quad.border_colors.right),
      hsla_to_rgba(quad.border_colors.bottom),
      hsla_to_rgba(quad.border_colors.left),
      gradient.solid,
      gradient.color0,
      gradient.color1,
      local_position,
      local_scale,
      {clip_distance.x, clip_distance.y, clip_distance.z, clip_distance.w}};
}

fragment float4 quad_fragment(QuadFragmentInput input [[stage_in]],
                              constant Quad *quads
                              [[buffer(QuadInputIndex_Quads)]]) {
  Quad quad = quads[input.quad_id];
  // Everything below is in the quad's own space, which is the window's when
  // nothing transforms the quad. `pixel` is the device pixels one unit of it covers.
  bool transformed = input.local_scale > 0.0;
  float pixel = transformed ? input.local_scale : 1.0;
  float2 local_position = transformed ? input.local_position : input.position.xy;
  float2 fill_position = local_position;
  Bounds_ScaledPixels fill_bounds = quad.bounds;
  float tile_alpha = 1.0;
  if (quad.background_tile.width > 0.0) {
    // The background is one tile, repeated or not, rather than the whole quad.
    BackgroundTile tile = quad.background_tile;
    float2 tile_origin = float2(tile.origin_x, tile.origin_y);
    float2 tile_size = float2(tile.width, tile.height);
    float2 pitch = float2(tile.pitch_x, tile.pitch_y);
    float2 local = fill_position - tile_origin;
    if (tile.repeat_x != 0u) {
      local.x -= pitch.x * floor(local.x / pitch.x);
    }
    if (tile.repeat_y != 0u) {
      local.y -= pitch.y * floor(local.y / pitch.y);
    }
    // Repeating axes have hard edges, so tiles meet without a seam; a single tile is antialiased.
    float coverage_x = tile.repeat_x != 0u
      ? (local.x < tile_size.x ? 1.0 : 0.0)
      : saturate(min(local.x, tile_size.x - local.x) * pixel + 0.5);
    float coverage_y = tile.repeat_y != 0u
      ? (local.y < tile_size.y ? 1.0 : 0.0)
      : saturate(min(local.y, tile_size.y - local.y) * pixel + 0.5);
    tile_alpha = coverage_x * coverage_y;
    fill_position = tile_origin + local;
    fill_bounds.origin.x = tile.origin_x;
    fill_bounds.origin.y = tile.origin_y;
    fill_bounds.size.width = tile.width;
    fill_bounds.size.height = tile.height;
  }
  float4 background_color = fill_color(quad.background, fill_position, fill_bounds,
    input.background_solid, input.background_color0, input.background_color1);
  background_color.a *= tile_alpha;
  float mask_alpha = content_mask_coverage(input.position.xy, quad.content_mask);
  if (transformed) {
    mask_alpha *= local_clip_coverage(local_position, quad.local_clip, pixel);
  }

  bool unrounded = quad.corner_radii.top_left == 0.0 &&
    quad.corner_radii.bottom_left == 0.0 &&
    quad.corner_radii.top_right == 0.0 &&
    quad.corner_radii.bottom_right == 0.0;

  // Fast path when the quad is not rounded and doesn't have any border. A
  // transformed quad has antialiased edges, which only the signed distance has.
  if (!transformed &&
      quad.border_widths.top == 0.0 &&
      quad.border_widths.left == 0.0 &&
      quad.border_widths.right == 0.0 &&
      quad.border_widths.bottom == 0.0 &&
      unrounded) {
    return background_color * float4(1.0, 1.0, 1.0, mask_alpha);
  }

  float2 size = float2(quad.bounds.size.width, quad.bounds.size.height);
  float2 half_size = size / 2.0;
  float2 point = local_position - float2(quad.bounds.origin.x, quad.bounds.origin.y);
  float2 center_to_point = point - half_size;

  // Signed distance field threshold for inclusion of pixels. 0.5 device pixels
  // is the minimum distance between the center of the pixel and the edge.
  const float antialias_threshold = 0.5 / pixel;

  // Horizontal and vertical radius of the nearest corner
  float2 corner_radius = pick_corner_radius(
    center_to_point, quad.corner_radii, quad.corner_radii_y);

  // Which side of each axis owns the point. A point inside a border belongs to
  // that border's side even when it is past the centre: a box as tall as its
  // top border has every row in the top border, and picking by the centre would
  // give the lower rows the 0 bottom width and paint them as background. Away
  // from both borders the centre decides, as it always has.
  bool is_left = point.x < quad.border_widths.left ||
    (center_to_point.x < 0.0 && point.x < size.x - quad.border_widths.right);
  bool is_top = point.y < quad.border_widths.top ||
    (center_to_point.y < 0.0 && point.y < size.y - quad.border_widths.bottom);

  // Width of the nearest borders
  float2 border = float2(
    is_left ? quad.border_widths.left : quad.border_widths.right,
    is_top ? quad.border_widths.top : quad.border_widths.bottom
  );

  // 0-width borders are reduced so that `inner_sdf >= antialias_threshold`.
  // The purpose of this is to not draw antialiasing pixels in this case.
  float2 reduced_border = float2(
    border.x == 0.0 ? -antialias_threshold : border.x,
    border.y == 0.0 ? -antialias_threshold : border.y);

  // Vector from the corner of the quad bounds to the point, after mirroring
  // the point into the bottom right quadrant. Both components are <= 0.
  float2 corner_to_point = fabs(center_to_point) - half_size;

  // Vector from the point to the center of the rounded corner's circle, also
  // mirrored into bottom right quadrant.
  float2 corner_center_to_point = corner_to_point + corner_radius;

  // Whether the nearest point on the border is rounded
  bool is_near_rounded_corner =
    corner_center_to_point.x >= 0.0 &&
    corner_center_to_point.y >= 0.0;

  // Vector from straight border inner corner to point.
  //
  // 0-width borders are turned into width -1 so that inner_sdf is > 1.0 near
  // the border. Without this, antialiasing pixels would be drawn.
  float2 straight_border_inner_corner_to_point = corner_to_point + reduced_border;

  // Whether the point is beyond the inner edge of the straight border
  bool is_beyond_inner_straight_border =
    straight_border_inner_corner_to_point.x > 0.0 ||
    straight_border_inner_corner_to_point.y > 0.0;


  // Whether the point is far enough inside the quad, such that the pixels are
  // not affected by the straight border.
  bool is_within_inner_straight_border =
    straight_border_inner_corner_to_point.x < -antialias_threshold &&
    straight_border_inner_corner_to_point.y < -antialias_threshold;

  // Fast path for points that must be part of the background
  if (is_within_inner_straight_border && !is_near_rounded_corner) {
    return background_color * float4(1.0, 1.0, 1.0, mask_alpha);
  }

  // Signed distance of the point to the outside edge of the quad's border
  float outer_sdf = quad_sdf_impl(corner_center_to_point, corner_radius);

  // Approximate signed distance of the point to the inside edge of the quad's
  // border. It is negative outside this edge (within the border), and
  // positive inside.
  //
  // This is not always an accurate signed distance:
  // * The rounded portions with varying border width use an approximation of
  //   nearest-point-on-ellipse.
  // * When it is quickly known to be outside the edge, -1.0 is used.
  float inner_sdf = 0.0;
  if (corner_center_to_point.x <= 0.0 || corner_center_to_point.y <= 0.0) {
    // Fast paths for straight borders
    inner_sdf = -max(straight_border_inner_corner_to_point.x,
                     straight_border_inner_corner_to_point.y);
  } else if (is_beyond_inner_straight_border) {
    // Fast path for points that must be outside the inner edge
    inner_sdf = -1.0 / pixel;
  } else if (reduced_border.x == reduced_border.y && corner_radius.x == corner_radius.y) {
    // Fast path for circular inner edge.
    inner_sdf = -(outer_sdf + reduced_border.x);
  } else {
    float2 ellipse_radii = max(float2(0.0), corner_radius - reduced_border);
    if (corner_radius.x == corner_radius.y) {
      inner_sdf = quarter_ellipse_sdf(corner_center_to_point, ellipse_radii);
    } else {
      inner_sdf = -ellipse_corner_sdf(corner_center_to_point, max(ellipse_radii, float2(1e-4)));
    }
  }

  // Negative when inside the border
  float border_sdf = max(inner_sdf, outer_sdf);

  float4 color = background_color;
  if (border_sdf < antialias_threshold) {
    float vertical_weight = border_vertical_weight(corner_to_point, border, pixel);
    float4 border_color = select_border_color(
      is_left, is_top, vertical_weight, input.border_color_top,
      input.border_color_right, input.border_color_bottom, input.border_color_left);

    // A side's style applies on its own side of the line that separates the
    // colors at a corner, whatever the style of the side it meets.
    bool vertical_owner = vertical_weight >= 0.5;
    uint style = vertical_owner
      ? (uint)(is_left ? quad.border_styles.left : quad.border_styles.right)
      : (uint)(is_top ? quad.border_styles.top : quad.border_styles.bottom);
    float owner_width = vertical_owner ? border.x : border.y;

    // Distances from the point to the outer and inner edge of the border. Their
    // sum is the border's thickness here, and half their difference is the
    // distance from its mid-line, positive on the outer half.
    float depth_from_outer = -outer_sdf;
    float depth_from_inner = -inner_sdf;

    if (style == BorderStyle_Dashed || style == BorderStyle_Dotted) {
      bool dotted = style == BorderStyle_Dotted;
      // Position along the perimeter in "dash space", where each dash
      // period has length 1
      float t = 0.0;

      // Total number of dash periods, so that the dash spacing can be
      // adjusted to evenly divide it
      float max_t = 0.0;

      // Border width is proportional to dash size. This is the behavior
      // used by browsers, but also avoids dashes from different segments
      // overlapping when dash size is smaller than the border width.
      //
      // Dash pattern: (2 * border width) dash, (1 * border width) gap.
      // Dots are one border width across, with a gap of one.
      const float dash_length_per_width = dotted ? 1.0 : 2.0;
      const float dash_gap_per_width = 1.0;
      const float dash_period_per_width = dash_length_per_width + dash_gap_per_width;

      // Since the dash size is determined by border width, the density of
      // dashes varies. Multiplying a pixel distance by this returns a
      // position in dash space - it has units (dash period / pixels). So
      // a dash velocity of (1 / 10) is 1 dash every 10 pixels.
      float dash_velocity = 0.0;

      // Dividing this by the border width gives the dash velocity
      const float dv_numerator = 1.0 / dash_period_per_width;

      if (unrounded) {
        // When corners aren't rounded, the dashes are separately laid
        // out on each straight line, rather than around the whole
        // perimeter. This way each line starts and ends with a dash.
        bool is_horizontal = corner_center_to_point.x < corner_center_to_point.y;

        // Choosing the right border width for dashed borders.
        // TODO: A better solution exists taking a look at the whole file.
        // this does not fix single dashed borders at the corners
        float2 dashed_border = float2(
        fmax(quad.border_widths.bottom, quad.border_widths.top),
        fmax(quad.border_widths.right, quad.border_widths.left));

        float border_width = is_horizontal ? dashed_border.x : dashed_border.y;
        dash_velocity = dv_numerator / border_width;
        t = is_horizontal ? point.x : point.y;
        t *= dash_velocity;
        max_t = is_horizontal ? size.x : size.y;
        max_t *= dash_velocity;
      } else {
        // When corners are rounded, the dashes are laid out clockwise
        // around the whole perimeter.

        // An elliptical corner is laid out along an arc of its mean radius, which
        // is exact for a circle and close enough for dashes on an ellipse.
        float2 r_tr = pick_corner_radius(float2(1.0, -1.0), quad.corner_radii, quad.corner_radii_y);
        float2 r_br = pick_corner_radius(float2(1.0, 1.0), quad.corner_radii, quad.corner_radii_y);
        float2 r_bl = pick_corner_radius(float2(-1.0, 1.0), quad.corner_radii, quad.corner_radii_y);
        float2 r_tl = pick_corner_radius(float2(-1.0, -1.0), quad.corner_radii, quad.corner_radii_y);

        float w_t = quad.border_widths.top;
        float w_r = quad.border_widths.right;
        float w_b = quad.border_widths.bottom;
        float w_l = quad.border_widths.left;

        // Straight side dash velocities
        float dv_t = w_t <= 0.0 ? 0.0 : dv_numerator / w_t;
        float dv_r = w_r <= 0.0 ? 0.0 : dv_numerator / w_r;
        float dv_b = w_b <= 0.0 ? 0.0 : dv_numerator / w_b;
        float dv_l = w_l <= 0.0 ? 0.0 : dv_numerator / w_l;

        // Straight side lengths in dash space
        float s_t = (size.x - r_tl.x - r_tr.x) * dv_t;
        float s_r = (size.y - r_tr.y - r_br.y) * dv_r;
        float s_b = (size.x - r_br.x - r_bl.x) * dv_b;
        float s_l = (size.y - r_bl.y - r_tl.y) * dv_l;

        float corner_dash_velocity_tr = corner_dash_velocity(dv_t, dv_r);
        float corner_dash_velocity_br = corner_dash_velocity(dv_b, dv_r);
        float corner_dash_velocity_bl = corner_dash_velocity(dv_b, dv_l);
        float corner_dash_velocity_tl = corner_dash_velocity(dv_t, dv_l);

        // Corner lengths in dash space
        float c_tr = (r_tr.x + r_tr.y) * 0.5 * (M_PI_F / 2.0) * corner_dash_velocity_tr;
        float c_br = (r_br.x + r_br.y) * 0.5 * (M_PI_F / 2.0) * corner_dash_velocity_br;
        float c_bl = (r_bl.x + r_bl.y) * 0.5 * (M_PI_F / 2.0) * corner_dash_velocity_bl;
        float c_tl = (r_tl.x + r_tl.y) * 0.5 * (M_PI_F / 2.0) * corner_dash_velocity_tl;

        // Cumulative dash space upto each segment
        float upto_tr = s_t;
        float upto_r = upto_tr + c_tr;
        float upto_br = upto_r + s_r;
        float upto_b = upto_br + c_br;
        float upto_bl = upto_b + s_b;
        float upto_l = upto_bl + c_bl;
        float upto_tl = upto_l + s_l;
        max_t = upto_tl + c_tl;

        if (is_near_rounded_corner) {
          float radians = atan2(corner_center_to_point.y, corner_center_to_point.x);
          float corner_t = radians * (corner_radius.x + corner_radius.y) * 0.5;

          if (center_to_point.x >= 0.0) {
            if (center_to_point.y < 0.0) {
              dash_velocity = corner_dash_velocity_tr;
              // Subtracted because radians is pi/2 to 0 when
              // going clockwise around the top right corner,
              // since the y axis has been flipped
              t = upto_r - corner_t * dash_velocity;
            } else {
              dash_velocity = corner_dash_velocity_br;
              // Added because radians is 0 to pi/2 when going
              // clockwise around the bottom-right corner
              t = upto_br + corner_t * dash_velocity;
            }
          } else {
            if (center_to_point.y >= 0.0) {
              dash_velocity = corner_dash_velocity_bl;
              // Subtracted because radians is pi/1 to 0 when
              // going clockwise around the bottom-left corner,
              // since the x axis has been flipped
              t = upto_l - corner_t * dash_velocity;
            } else {
              dash_velocity = corner_dash_velocity_tl;
              // Added because radians is 0 to pi/2 when going
              // clockwise around the top-left corner, since both
              // axis were flipped
              t = upto_tl + corner_t * dash_velocity;
            }
          }
        } else {
          // Straight borders
          bool is_horizontal = corner_center_to_point.x < corner_center_to_point.y;
          if (is_horizontal) {
            if (is_top) {
              dash_velocity = dv_t;
              t = (point.x - r_tl.x) * dash_velocity;
            } else {
              dash_velocity = dv_b;
              t = upto_bl - (point.x - r_bl.x) * dash_velocity;
            }
          } else {
            if (is_left) {
              dash_velocity = dv_l;
              t = upto_tl - (point.y - r_tl.y) * dash_velocity;
            } else {
              dash_velocity = dv_r;
              t = upto_r + (point.y - r_tr.y) * dash_velocity;
            }
          }
        }
      }

      float dash_length = dash_length_per_width / dash_period_per_width;
      float desired_dash_gap = dash_gap_per_width / dash_period_per_width;

      // Straight borders should start and end with a dash, so max_t is
      // reduced to cause this.
      max_t -= unrounded ? dash_length : 0.0;
      if (max_t >= 1.0) {
        // Adjust dash gap to evenly divide max_t
        float dash_count = floor(max_t);
        float dash_period = max_t / dash_count;
        border_color.a *= dotted
          ? dot_alpha(t, dash_period, dash_length, dash_velocity,
                      (depth_from_inner - depth_from_outer) / 2.0, owner_width,
                      antialias_threshold, pixel)
          : dash_alpha(t, dash_period, dash_length, dash_velocity,
                       antialias_threshold, pixel);
      } else if (unrounded) {
        // When there isn't enough space for the full gap between the
        // two start / end dashes of a straight border, reduce gap to
        // make them fit.
        float dash_gap = max_t - dash_length;
        if (dash_gap > 0.0) {
          float dash_period = dash_length + dash_gap;
          border_color.a *= dotted
            ? dot_alpha(t, dash_period, dash_length, dash_velocity,
                        (depth_from_inner - depth_from_outer) / 2.0, owner_width,
                        antialias_threshold, pixel)
            : dash_alpha(t, dash_period, dash_length, dash_velocity,
                         antialias_threshold, pixel);
        }
      }
    } else if (style == BorderStyle_Double) {
      // Two lines of a third of the width each, rounded to whole pixels as
      // Chrome does, with the rest as the gap.
      float line_width = floor((owner_width + 1.0) / 3.0);
      float outer_line = saturate((antialias_threshold - (depth_from_outer - line_width)) * pixel);
      float inner_line = saturate((antialias_threshold - (depth_from_inner - line_width)) * pixel);
      border_color.a *= max(outer_line, inner_line);
    } else if (style == BorderStyle_Groove || style == BorderStyle_Ridge) {
      // The border cut in two along its mid-line: `border_color` is the outer
      // half and the inner half has its own color.
      float4 inner_color = select_border_color(
        is_left, is_top, vertical_weight,
        hsla_to_rgba(quad.border_inner_colors.top),
        hsla_to_rgba(quad.border_inner_colors.right),
        hsla_to_rgba(quad.border_inner_colors.bottom),
        hsla_to_rgba(quad.border_inner_colors.left));
      float outer_half = floor((owner_width + 1.0) / 2.0);
      float outer_weight = saturate((antialias_threshold + outer_half - depth_from_outer) * pixel);
      border_color = mix(inner_color, border_color, outer_weight);
    }

    // Blend the border on top of the background and then linearly interpolate
    // between the two as we slide inside the background.
    float4 blended_border = over(background_color, border_color);
    color = mix(background_color, blended_border,
                saturate((antialias_threshold - inner_sdf) * pixel));
  }

  return color * float4(1.0, 1.0, 1.0, saturate((antialias_threshold - outer_sdf) * pixel) * mask_alpha);
}

// How much of the point's corner belongs to the vertical (left or right) side
// rather than the horizontal one: 1 on the vertical side, 0 on the horizontal
// one, antialiased across the join line. Like CSS, two sides meet at each corner
// along the line from the outer corner of the quad to the inner corner of its
// borders.
//
// `border` holds the widths of the nearest vertical and horizontal sides.
float border_vertical_weight(float2 corner_to_point, float2 border, float pixel) {
  // A side without a border never owns the corner.
  if (border.x <= 0.0) {
    return 0.0;
  }
  if (border.y <= 0.0) {
    return 1.0;
  }
  // Signed distance to the join line, negative on the vertical side's half.
  float2 distance_from_edges = -corner_to_point;
  float to_join = (distance_from_edges.x * border.y - distance_from_edges.y * border.x) /
                  length(border);
  return saturate(0.5 - to_join * pixel);
}

// Picks the border color at a point, blending the two sides across the join line.
float4 select_border_color(bool is_left, bool is_top, float vertical_weight,
                           float4 top, float4 right, float4 bottom, float4 left) {
  float4 vertical_color = is_left ? left : right;
  float4 horizontal_color = is_top ? top : bottom;

  if (vertical_weight >= 1.0) {
    return vertical_color;
  }
  if (vertical_weight <= 0.0) {
    return horizontal_color;
  }
  // Blend premultiplied so a transparent side doesn't darken the join.
  float4 premultiplied = mix(
    float4(horizontal_color.rgb * horizontal_color.a, horizontal_color.a),
    float4(vertical_color.rgb * vertical_color.a, vertical_color.a),
    vertical_weight);
  if (premultiplied.a <= 0.0) {
    return float4(0.0);
  }
  return float4(premultiplied.rgb / premultiplied.a, premultiplied.a);
}

// Coverage of the point by the content mask's rounded corners. The mask's
// rectangle is already enforced by the clip distances, so this only matters
// when a corner is rounded.
float content_mask_coverage(float2 position, ContentMask_ScaledPixels content_mask) {
  Corners_ScaledPixels radii = content_mask.corner_radii;
  if (radii.top_left == 0.0 && radii.top_right == 0.0 &&
      radii.bottom_right == 0.0 && radii.bottom_left == 0.0) {
    return 1.0;
  }
  return saturate(0.5 - quad_sdf(position, content_mask.bounds, radii, content_mask.corner_radii_y));
}

// Returns the dash velocity of a corner given the dash velocity of the two
// sides, by returning the slower velocity (larger dashes).
//
// Since 0 is used for dash velocity when the border width is 0 (instead of
// +inf), this returns the other dash velocity in that case.
//
// An alternative to this might be to appropriately interpolate the dash
// velocity around the corner, but that seems overcomplicated.
float corner_dash_velocity(float dv1, float dv2) {
  if (dv1 == 0.0) {
    return dv2;
  } else if (dv2 == 0.0) {
    return dv1;
  } else {
    return min(dv1, dv2);
  }
}

// Returns alpha used to render antialiased dashes.
// `t` is within the dash when `fmod(t, period) < length`.
float dash_alpha(
    float t, float period, float length, float dash_velocity,
    float antialias_threshold, float pixel) {
  float half_period = period / 2.0;
  float half_length = length / 2.0;
  // Value in [-half_period, half_period]
  // The dash is in [-half_length, half_length]
  float centered = fmod(t + half_period - half_length, period) - half_period;
  // Signed distance for the dash, negative values are inside the dash
  float signed_distance = abs(centered) - half_length;
  // Antialiased alpha based on the signed distance
  return saturate((antialias_threshold - signed_distance / dash_velocity) * pixel);
}

// Returns alpha used to render antialiased dots: circles one border width across,
// centered on the border's mid-line and on the middle of each dash cell of `t`
// (see `dash_alpha`). `across` is the distance from the mid-line, in pixels.
float dot_alpha(
    float t, float period, float dot_length, float dash_velocity,
    float across, float diameter, float antialias_threshold, float pixel) {
  float half_period = period / 2.0;
  float half_length = dot_length / 2.0;
  // Dash space distance from the middle of the dot, converted to pixels
  float centered = fmod(t + half_period - half_length, period) - half_period;
  float along = centered / dash_velocity;
  float signed_distance = length(float2(along, across)) - diameter / 2.0;
  return saturate((antialias_threshold - signed_distance) * pixel);
}

// This approximates distance to the nearest point to a quarter ellipse in a way
// that is sufficient for anti-aliasing when the ellipse is not very eccentric.
// The components of `point` are expected to be positive.
//
// Negative on the outside and positive on the inside.
float quarter_ellipse_sdf(float2 point, float2 radii) {
  // Scale the space to treat the ellipse like a unit circle
  float2 circle_vec = point / radii;
  float unit_circle_sdf = length(circle_vec) - 1.0;
  // Approximate up-scaling of the length by using the average of the radii.
  //
  // TODO: A better solution would be to use the gradient of the implicit
  // function for an ellipse to approximate a scaling factor.
  return unit_circle_sdf * (radii.x + radii.y) * -0.5;
}

// Distance from a point in the corner of an elliptical quad to the quarter
// ellipse, positive outside and negative inside. `corner_center_to_point` is
// the vector from the ellipse's center and both of its components are positive.
// The unit circle distance is scaled by the gradient of the ellipse's implicit
// function, which is exact for a circle and close to the real distance near
// the edge for an eccentric ellipse.
float ellipse_corner_sdf(float2 corner_center_to_point, float2 radii) {
  float k0 = length(corner_center_to_point / radii);
  float k1 = length(corner_center_to_point / (radii * radii));
  return k0 * (k0 - 1.0) / k1;
}

struct ShadowVertexOutput {
  float4 position [[position]];
  float4 color [[flat]];
  uint shadow_id [[flat]];
  float2 local_position;
  float local_scale [[flat]];
  float clip_distance [[clip_distance]][4];
};

struct ShadowFragmentInput {
  float4 position [[position]];
  float4 color [[flat]];
  uint shadow_id [[flat]];
  float2 local_position;
  float local_scale [[flat]];
};

vertex ShadowVertexOutput shadow_vertex(
    uint unit_vertex_id [[vertex_id]], uint shadow_id [[instance_id]],
    constant float2 *unit_vertices [[buffer(ShadowInputIndex_Vertices)]],
    constant Shadow *shadows [[buffer(ShadowInputIndex_Shadows)]],
    constant Size_DevicePixels *viewport_size
    [[buffer(ShadowInputIndex_ViewportSize)]]) {
  float2 unit_vertex = unit_vertices[unit_vertex_id];
  Shadow shadow = shadows[shadow_id];

  Bounds_ScaledPixels bounds;
  if (shadow.inset != 0u) {
    bounds = shadow.element_bounds;
  } else {
    // Leave room for the gaussian tail outside the shadow rect.
    float margin = 3. * shadow.blur_radius;
    bounds = shadow.bounds;
    bounds.origin.x -= margin;
    bounds.origin.y -= margin;
    bounds.size.width += 2. * margin;
    bounds.size.height += 2. * margin;
  }

  float4 device_position;
  float4 clip_distance;
  float2 local_position = float2(0.0);
  float local_scale = 0.0;
  if (is_identity_transform(shadow.transformation)) {
    device_position = to_device_position(unit_vertex, bounds, viewport_size);
    clip_distance =
        distance_from_clip_rect(unit_vertex, bounds, shadow.content_mask.bounds);
  } else {
    local_scale = transform_pixel_scale(shadow.transformation);
    float pad = 1.0 / local_scale;
    float2 origin = float2(bounds.origin.x, bounds.origin.y) - pad;
    float2 extent = float2(bounds.size.width, bounds.size.height) + 2.0 * pad;
    local_position = origin + unit_vertex * extent;
    float2 window_position = apply_transform(shadow.transformation, local_position);
    device_position = device_position_of(window_position, viewport_size);
    clip_distance = distance_from_clip(window_position, shadow.content_mask.bounds);
  }
  float4 color = hsla_to_rgba(shadow.color);

  return ShadowVertexOutput{
      device_position,
      color,
      shadow_id,
      local_position,
      local_scale,
      {clip_distance.x, clip_distance.y, clip_distance.z, clip_distance.w}};
}

fragment float4 shadow_fragment(ShadowFragmentInput input [[stage_in]],
                                constant Shadow *shadows
                                [[buffer(ShadowInputIndex_Shadows)]]) {
  Shadow shadow = shadows[input.shadow_id];
  bool transformed = input.local_scale > 0.0;
  float pixel = transformed ? input.local_scale : 1.0;
  float2 local_position = transformed ? input.local_position : input.position.xy;

  float2 origin = float2(shadow.bounds.origin.x, shadow.bounds.origin.y);
  float2 size = float2(shadow.bounds.size.width, shadow.bounds.size.height);
  float2 half_size = size / 2.;
  float2 center = origin + half_size;
  float2 point = local_position - center;
  float2 corner_radius = pick_corner_radius(point, shadow.corner_radii, shadow.corner_radii_y);

  float alpha;
  if (shadow.blur_radius == 0.) {
    float distance = quad_sdf(local_position, shadow.bounds, shadow.corner_radii,
                              shadow.corner_radii_y);
    alpha = saturate(0.5 - distance * pixel);
  } else {
    // The signal is only non-zero in a limited range, so don't waste samples
    float low = point.y - half_size.y;
    float high = point.y + half_size.y;
    float start = clamp(-3. * shadow.blur_radius, low, high);
    float end = clamp(3. * shadow.blur_radius, low, high);

    // Accumulate samples (we can get away with surprisingly few samples)
    float step = (end - start) / 4.;
    float y = start + step * 0.5;
    alpha = 0.;
    for (int i = 0; i < 4; i++) {
      alpha += blur_along_x(point.x, point.y - y, shadow.blur_radius,
                            corner_radius, half_size) *
               gaussian(y, shadow.blur_radius) * step;
      y += step;
    }
  }

  // `saturate(0.5 - d)` gives a 1-pixel antialiased edge: d <= -0.5 -> 1, d >= 0.5 -> 0.
  float element_distance = quad_sdf(local_position, shadow.element_bounds,
                                    shadow.element_corner_radii,
                                    shadow.element_corner_radii_y);
  float inside_element = saturate(0.5 - element_distance * pixel);
  if (shadow.inset != 0u) {
    // The complement of the (blurred) hole rect, kept inside the element's padding box.
    alpha = (1. - alpha) * inside_element;
  } else {
    // CSS Backgrounds 3 7.1.1: an outer shadow is only painted outside the border box.
    alpha *= 1. - inside_element;
  }

  alpha *= content_mask_coverage(input.position.xy, shadow.content_mask);
  if (transformed) {
    alpha *= local_clip_coverage(local_position, shadow.local_clip, pixel);
  }
  return input.color * float4(1., 1., 1., alpha);
}

struct UnderlineVertexOutput {
  float4 position [[position]];
  float4 color [[flat]];
  uint underline_id [[flat]];
  float2 local_position;
  float local_scale [[flat]];
  float clip_distance [[clip_distance]][4];
};

struct UnderlineFragmentInput {
  float4 position [[position]];
  float4 color [[flat]];
  uint underline_id [[flat]];
  float2 local_position;
  float local_scale [[flat]];
};

vertex UnderlineVertexOutput underline_vertex(
    uint unit_vertex_id [[vertex_id]], uint underline_id [[instance_id]],
    constant float2 *unit_vertices [[buffer(UnderlineInputIndex_Vertices)]],
    constant Underline *underlines [[buffer(UnderlineInputIndex_Underlines)]],
    constant Size_DevicePixels *viewport_size
    [[buffer(ShadowInputIndex_ViewportSize)]]) {
  float2 unit_vertex = unit_vertices[unit_vertex_id];
  Underline underline = underlines[underline_id];
  TransformedVertex vertex_data =
      transformed_vertex(unit_vertex, underline.bounds, underline.transformation,
                         underline.content_mask.bounds, viewport_size);
  float4 clip_distance = vertex_data.clip_distance;
  float4 color = hsla_to_rgba(underline.color);
  return UnderlineVertexOutput{
      vertex_data.device_position,
      color,
      underline_id,
      vertex_data.local_position,
      vertex_data.local_scale,
      {clip_distance.x, clip_distance.y, clip_distance.z, clip_distance.w}};
}

fragment float4 underline_fragment(UnderlineFragmentInput input [[stage_in]],
                                   constant Underline *underlines
                                   [[buffer(UnderlineInputIndex_Underlines)]]) {
  const float WAVE_FREQUENCY = 2.0;
  const float WAVE_HEIGHT_RATIO = 0.8;

  // The numbers of `SHAPE_*` in gpui's text_decoration.rs.
  const uint SHAPE_DOTTED = 1u;
  const uint SHAPE_DASHED = 2u;
  const uint SHAPE_WAVY = 3u;

  Underline underline = underlines[input.underline_id];
  // Everything below is in the underline's own space, which is the window's when nothing
  // transforms it. `pixel` is the device pixels one unit of it covers.
  bool transformed = input.local_scale > 0.0;
  float pixel = transformed ? input.local_scale : 1.0;
  float2 local_position = transformed ? input.local_position : input.position.xy;
  float mask_alpha = content_mask_coverage(input.position.xy, underline.content_mask);
  if (transformed) {
    mask_alpha *= local_clip_coverage(local_position, underline.local_clip, pixel) *
                  rect_coverage(local_position, underline.bounds, pixel);
  }
  if (underline.style == SHAPE_DOTTED) {
    // Round dots, laid out from the left edge; the nearest dot decides the coverage.
    float radius = underline.mark_length * 0.5;
    float2 local = local_position - float2(underline.bounds.origin.x, underline.bounds.origin.y);
    float nearest = floor((local.x - radius) / underline.mark_period + 0.5);
    float2 center = float2(nearest * underline.mark_period + radius, radius);
    float alpha = saturate(0.5 - (length(local - center) - radius) * pixel);
    return input.color * float4(1., 1., 1., alpha * mask_alpha);
  } else if (underline.style == SHAPE_DASHED) {
    float x = local_position.x - underline.bounds.origin.x;
    float along = x - floor(x / underline.mark_period) * underline.mark_period;
    float alpha = saturate(along * pixel + 0.5) *
                  saturate((underline.mark_length - along) * pixel + 0.5);
    return input.color * float4(1., 1., 1., alpha * mask_alpha);
  } else if (underline.style == SHAPE_WAVY) {
    float half_thickness = underline.thickness * 0.5;
    float2 origin =
        float2(underline.bounds.origin.x, underline.bounds.origin.y);

    float2 st = ((local_position - origin) / underline.bounds.size.height) -
                float2(0., 0.5);
    float frequency = (M_PI_F * WAVE_FREQUENCY * underline.thickness) / underline.bounds.size.height;
    float amplitude = (underline.thickness * WAVE_HEIGHT_RATIO) / underline.bounds.size.height;

    float sine = sin(st.x * frequency) * amplitude;
    float dSine = cos(st.x * frequency) * amplitude * frequency;
    float distance = (st.y - sine) / sqrt(1. + dSine * dSine);
    float distance_in_pixels = distance * underline.bounds.size.height;
    float distance_from_top_border = distance_in_pixels - half_thickness;
    float distance_from_bottom_border = distance_in_pixels + half_thickness;
    float alpha = saturate(
        0.5 - max(-distance_from_bottom_border, distance_from_top_border) * pixel);
    return input.color * float4(1., 1., 1., alpha * mask_alpha);
  } else {
    return input.color * float4(1., 1., 1., mask_alpha);
  }
}

struct MonochromeSpriteVertexOutput {
  float4 position [[position]];
  float2 tile_position;
  uint sprite_id [[flat]];
  float4 color [[flat]];
  float4 clip_distance;
  float2 local_position;
  float local_scale [[flat]];
};

struct MonochromeSpriteFragmentInput {
  float4 position [[position]];
  float2 tile_position;
  uint sprite_id [[flat]];
  float4 color [[flat]];
  float4 clip_distance;
  float2 local_position;
  float local_scale [[flat]];
};

vertex MonochromeSpriteVertexOutput monochrome_sprite_vertex(
    uint unit_vertex_id [[vertex_id]], uint sprite_id [[instance_id]],
    constant float2 *unit_vertices [[buffer(SpriteInputIndex_Vertices)]],
    constant MonochromeSprite *sprites [[buffer(SpriteInputIndex_Sprites)]],
    constant Size_DevicePixels *viewport_size
    [[buffer(SpriteInputIndex_ViewportSize)]],
    constant Size_DevicePixels *atlas_size
    [[buffer(SpriteInputIndex_AtlasTextureSize)]]) {
  float2 unit_vertex = unit_vertices[unit_vertex_id];
  MonochromeSprite sprite = sprites[sprite_id];
  TransformedVertex vertex_data =
      transformed_vertex(unit_vertex, sprite.bounds, sprite.transformation,
                         sprite.content_mask.bounds, viewport_size);
  float4 clip_distance = vertex_data.clip_distance;
  float2 tile_position = to_tile_position(vertex_data.unit_position, sprite.tile, atlas_size);
  float4 color = hsla_to_rgba(sprite.color);
  return MonochromeSpriteVertexOutput{
      vertex_data.device_position,
      tile_position,
      sprite_id,
      color,
      {clip_distance.x, clip_distance.y, clip_distance.z, clip_distance.w},
      vertex_data.local_position,
      vertex_data.local_scale};
}

fragment float4 monochrome_sprite_fragment(
    MonochromeSpriteFragmentInput input [[stage_in]],
    constant MonochromeSprite *sprites [[buffer(SpriteInputIndex_Sprites)]],
    texture2d<float> atlas_texture [[texture(SpriteInputIndex_AtlasTexture)]]) {
  if (any(input.clip_distance < float4(0.0))) {
    return float4(0.0);
  }

  MonochromeSprite sprite = sprites[input.sprite_id];
  constexpr sampler atlas_texture_sampler(mag_filter::linear,
                                          min_filter::linear);
  float2 tile_position = input.tile_position;
  float coverage = 1.0;
  if (input.local_scale > 0.0) {
    // The padding around a transformed sprite reads the tile's edge texels, not its
    // neighbours in the atlas, and is cut to the sprite's antialiased bounds.
    float2 atlas_size = float2(atlas_texture.get_width(), atlas_texture.get_height());
    float2 tile_origin = float2(sprite.tile.bounds.origin.x, sprite.tile.bounds.origin.y);
    float2 tile_size = float2(sprite.tile.bounds.size.width, sprite.tile.bounds.size.height);
    float2 tile_low = (tile_origin + 0.5) / atlas_size;
    float2 tile_high = max((tile_origin + tile_size - 0.5) / atlas_size, tile_low);
    tile_position = clamp(tile_position, tile_low, tile_high);
    coverage = rect_coverage(input.local_position, sprite.bounds, input.local_scale) *
               local_clip_coverage(input.local_position, sprite.local_clip, input.local_scale);
  } else if (!is_identity_transform(sprite.clip_transformation)) {
    // A sprite drawn upright inside a transform that clips: the clip is in the transform's
    // local space, found from the window position of the fragment.
    coverage = local_clip_coverage(
        apply_transform(sprite.clip_transformation, input.position.xy), sprite.local_clip,
        1.0 / transform_pixel_scale(sprite.clip_transformation));
  }
  float4 sample =
      atlas_texture.sample(atlas_texture_sampler, tile_position);
  float4 color = input.color;
  color.a *= sample.a * coverage;
  color.a *= content_mask_coverage(input.position.xy, sprite.content_mask);
  return color;
}

struct PolychromeSpriteVertexOutput {
  float4 position [[position]];
  float2 tile_position;
  uint sprite_id [[flat]];
  float2 local_position;
  float local_scale [[flat]];
  float clip_distance [[clip_distance]][4];
};

struct PolychromeSpriteFragmentInput {
  float4 position [[position]];
  float2 tile_position;
  uint sprite_id [[flat]];
  float2 local_position;
  float local_scale [[flat]];
};

vertex PolychromeSpriteVertexOutput polychrome_sprite_vertex(
    uint unit_vertex_id [[vertex_id]], uint sprite_id [[instance_id]],
    constant float2 *unit_vertices [[buffer(SpriteInputIndex_Vertices)]],
    constant PolychromeSprite *sprites [[buffer(SpriteInputIndex_Sprites)]],
    constant Size_DevicePixels *viewport_size
    [[buffer(SpriteInputIndex_ViewportSize)]],
    constant Size_DevicePixels *atlas_size
    [[buffer(SpriteInputIndex_AtlasTextureSize)]]) {

  float2 unit_vertex = unit_vertices[unit_vertex_id];
  PolychromeSprite sprite = sprites[sprite_id];
  TransformedVertex vertex_data =
      transformed_vertex(unit_vertex, sprite.bounds, sprite.transformation,
                         sprite.content_mask.bounds, viewport_size);
  float4 clip_distance = vertex_data.clip_distance;
  float2 tile_position = to_tile_position(vertex_data.unit_position, sprite.tile, atlas_size);
  return PolychromeSpriteVertexOutput{
      vertex_data.device_position,
      tile_position,
      sprite_id,
      vertex_data.local_position,
      vertex_data.local_scale,
      {clip_distance.x, clip_distance.y, clip_distance.z, clip_distance.w}};
}

fragment float4 polychrome_sprite_fragment(
    PolychromeSpriteFragmentInput input [[stage_in]],
    constant PolychromeSprite *sprites [[buffer(SpriteInputIndex_Sprites)]],
    texture2d<float> atlas_texture [[texture(SpriteInputIndex_AtlasTexture)]]) {
  PolychromeSprite sprite = sprites[input.sprite_id];
  // Everything below is in the sprite's own space, which is the window's when nothing
  // transforms it. `pixel` is the device pixels one unit of it covers.
  bool transformed = input.local_scale > 0.0;
  float pixel = transformed ? input.local_scale : 1.0;
  float2 local_position = transformed ? input.local_position : input.position.xy;
  constexpr sampler linear_sampler(mag_filter::linear, min_filter::linear);
  constexpr sampler nearest_neighbor_sampler(mag_filter::nearest,
                                             min_filter::nearest);
  // Linear filtering reads half a texel past the tile's edge, which is the next tile of the atlas,
  // or the rest of the image when the sprite is a crop of it (a slice of a nine-slice). Keep the
  // lookup on the tile's own texels.
  float2 atlas_size = float2(atlas_texture.get_width(), atlas_texture.get_height());
  float2 tile_origin = float2(sprite.tile.bounds.origin.x, sprite.tile.bounds.origin.y);
  float2 tile_size = float2(sprite.tile.bounds.size.width, sprite.tile.bounds.size.height);
  float2 tile_low = (tile_origin + 0.5) / atlas_size;
  float2 tile_high = max((tile_origin + tile_size - 0.5) / atlas_size, tile_low);
  float2 tile_position = clamp(input.tile_position, tile_low, tile_high);
  // The padding of a transformed sprite is past the tile for the nearest sampler too.
  float4 sample = sprite.nearest_neighbor
                      ? atlas_texture.sample(nearest_neighbor_sampler,
                                             transformed ? tile_position : input.tile_position)
                      : atlas_texture.sample(linear_sampler, tile_position);
  float distance =
      quad_sdf(local_position, sprite.bounds, sprite.corner_radii, sprite.corner_radii_y);

  float4 color = sample;
  if (sprite.grayscale) {
    float grayscale = 0.2126 * color.r + 0.7152 * color.g + 0.0722 * color.b;
    color.r = grayscale;
    color.g = grayscale;
    color.b = grayscale;
  }
  float clip_alpha = 1.0;
  if (transformed) {
    clip_alpha = local_clip_coverage(local_position, sprite.local_clip, pixel);
  } else if (!is_identity_transform(sprite.clip_transformation)) {
    clip_alpha = local_clip_coverage(
        apply_transform(sprite.clip_transformation, input.position.xy), sprite.local_clip,
        1.0 / transform_pixel_scale(sprite.clip_transformation));
  }
  color.a *= sprite.opacity * saturate(0.5 - distance * pixel) * clip_alpha *
             content_mask_coverage(input.position.xy, sprite.content_mask);
  return color;
}

struct PathRasterizationVertexOutput {
  float4 position [[position]];
  float2 st_position;
  uint vertex_id [[flat]];
  float clip_rect_distance [[clip_distance]][4];
};

struct PathRasterizationFragmentInput {
  float4 position [[position]];
  float2 st_position;
  uint vertex_id [[flat]];
};

vertex PathRasterizationVertexOutput path_rasterization_vertex(
  uint vertex_id [[vertex_id]],
  constant PathRasterizationVertex *vertices [[buffer(PathRasterizationInputIndex_Vertices)]],
  constant Size_DevicePixels *atlas_size [[buffer(PathRasterizationInputIndex_ViewportSize)]]
) {
  PathRasterizationVertex v = vertices[vertex_id];
  float2 vertex_position = float2(v.xy_position.x, v.xy_position.y);
  float4 position = float4(
    vertex_position * float2(2. / atlas_size->width, -2. / atlas_size->height) + float2(-1., 1.),
    0.,
    1.
  );
  return PathRasterizationVertexOutput{
      position,
      float2(v.st_position.x, v.st_position.y),
      vertex_id,
      {
        v.xy_position.x - v.bounds.origin.x,
        v.bounds.origin.x + v.bounds.size.width - v.xy_position.x,
        v.xy_position.y - v.bounds.origin.y,
        v.bounds.origin.y + v.bounds.size.height - v.xy_position.y
      }
  };
}

fragment float4 path_rasterization_fragment(
  PathRasterizationFragmentInput input [[stage_in]],
  constant PathRasterizationVertex *vertices [[buffer(PathRasterizationInputIndex_Vertices)]]
) {
  float2 dx = dfdx(input.st_position);
  float2 dy = dfdy(input.st_position);

  PathRasterizationVertex v = vertices[input.vertex_id];
  Background background = v.color;
  Bounds_ScaledPixels path_bounds = v.bounds;
  float alpha;
  if (length(float2(dx.x, dy.x)) < 0.001) {
    alpha = 1.0;
  } else {
    float2 gradient = float2(
      (2. * input.st_position.x) * dx.x - dx.y,
      (2. * input.st_position.x) * dy.x - dy.y
    );
    float f = (input.st_position.x * input.st_position.x) - input.st_position.y;
    float distance = f / length(gradient);
    alpha = saturate(0.5 - distance);
  }

  GradientColor gradient_color = prepare_fill_color(
    background.tag,
    background.color_space,
    background.solid,
    background.colors[0].color,
    background.colors[1].color
  );

  float4 color = fill_color(
    background,
    input.position.xy,
    path_bounds,
    gradient_color.solid,
    gradient_color.color0,
    gradient_color.color1
  );
  alpha *= content_mask_coverage(input.position.xy, v.content_mask);
  return float4(color.rgb * color.a * alpha, alpha * color.a);
}

struct PathSpriteVertexOutput {
  float4 position [[position]];
  float2 texture_coords;
};

vertex PathSpriteVertexOutput path_sprite_vertex(
  uint unit_vertex_id [[vertex_id]],
  uint sprite_id [[instance_id]],
  constant float2 *unit_vertices [[buffer(SpriteInputIndex_Vertices)]],
  constant PathSprite *sprites [[buffer(SpriteInputIndex_Sprites)]],
  constant Size_DevicePixels *viewport_size [[buffer(SpriteInputIndex_ViewportSize)]]
) {
  float2 unit_vertex = unit_vertices[unit_vertex_id];
  PathSprite sprite = sprites[sprite_id];
  // Don't apply content mask because it was already accounted for when
  // rasterizing the path.
  float4 device_position =
      to_device_position(unit_vertex, sprite.bounds, viewport_size);

  float2 screen_position = float2(sprite.bounds.origin.x, sprite.bounds.origin.y) + unit_vertex * float2(sprite.bounds.size.width, sprite.bounds.size.height);
  float2 texture_coords = screen_position / float2(viewport_size->width, viewport_size->height);

  return PathSpriteVertexOutput{
    device_position,
    texture_coords
  };
}

fragment float4 path_sprite_fragment(
  PathSpriteVertexOutput input [[stage_in]],
  texture2d<float> intermediate_texture [[texture(SpriteInputIndex_AtlasTexture)]]
) {
  constexpr sampler intermediate_texture_sampler(mag_filter::linear, min_filter::linear);
  return intermediate_texture.sample(intermediate_texture_sampler, input.texture_coords);
}

struct SurfaceVertexOutput {
  float4 position [[position]];
  float2 texture_position;
  uint surface_id [[flat]];
  float2 local_position;
  float local_scale;
  float clip_distance [[clip_distance]][4];
};

struct SurfaceFragmentInput {
  float4 position [[position]];
  float2 texture_position;
  uint surface_id [[flat]];
  float2 local_position;
  float local_scale;
};

vertex SurfaceVertexOutput surface_vertex(
    uint unit_vertex_id [[vertex_id]], uint surface_id [[instance_id]],
    constant float2 *unit_vertices [[buffer(SurfaceInputIndex_Vertices)]],
    constant SurfaceBounds *surfaces [[buffer(SurfaceInputIndex_Surfaces)]],
    constant Size_DevicePixels *viewport_size
    [[buffer(SurfaceInputIndex_ViewportSize)]],
    constant Size_DevicePixels *texture_size
    [[buffer(SurfaceInputIndex_TextureSize)]]) {
  float2 unit_vertex = unit_vertices[unit_vertex_id];
  SurfaceBounds surface = surfaces[surface_id];
  TransformedVertex vertex_data =
      transformed_vertex(unit_vertex, surface.bounds, surface.transformation,
                         surface.content_mask.bounds, viewport_size);
  float4 clip_distance = vertex_data.clip_distance;
  // We are going to copy the whole texture, so the texture position corresponds
  // to the position across the bounds (a little beyond them for the padding of a
  // transformed surface, which the fragment clamps).
  return SurfaceVertexOutput{
      vertex_data.device_position,
      vertex_data.unit_position,
      surface_id,
      vertex_data.local_position,
      vertex_data.local_scale,
      {clip_distance.x, clip_distance.y, clip_distance.z, clip_distance.w}};
}

fragment float4 surface_fragment(SurfaceFragmentInput input [[stage_in]],
                                 constant SurfaceBounds *surfaces
                                 [[buffer(SurfaceInputIndex_Surfaces)]],
                                 texture2d<float> y_texture
                                 [[texture(SurfaceInputIndex_YTexture)]],
                                 texture2d<float> cb_cr_texture
                                 [[texture(SurfaceInputIndex_CbCrTexture)]]) {
  constexpr sampler texture_sampler(mag_filter::linear, min_filter::linear);
  const float4x4 ycbcrToRGBTransform =
      float4x4(float4(+1.0000f, +1.0000f, +1.0000f, +0.0000f),
               float4(+0.0000f, -0.3441f, +1.7720f, +0.0000f),
               float4(+1.4020f, -0.7141f, +0.0000f, +0.0000f),
               float4(-0.7010f, +0.5291f, -0.8860f, +1.0000f));
  SurfaceBounds surface = surfaces[input.surface_id];
  bool transformed = input.local_scale > 0.0;
  float2 texture_position = transformed ? saturate(input.texture_position) : input.texture_position;
  float4 ycbcr = float4(
      y_texture.sample(texture_sampler, texture_position).r,
      cb_cr_texture.sample(texture_sampler, texture_position).rg, 1.0);

  float4 color = ycbcrToRGBTransform * ycbcr;
  float coverage = content_mask_coverage(input.position.xy, surface.content_mask);
  if (transformed) {
    // The padding past the bounds is where the antialiased edge is drawn.
    coverage *= rect_coverage(input.local_position, surface.bounds, input.local_scale) *
                local_clip_coverage(input.local_position, surface.local_clip, input.local_scale);
  }
  color.a = coverage;
  return color;
}

float4 hsla_to_rgba(Hsla hsla) {
  float h = hsla.h * 6.0; // Now, it's an angle but scaled in [0, 6) range
  float s = hsla.s;
  float l = hsla.l;
  float a = hsla.a;

  float c = (1.0 - fabs(2.0 * l - 1.0)) * s;
  float x = c * (1.0 - fabs(fmod(h, 2.0) - 1.0));
  float m = l - c / 2.0;

  float r = 0.0;
  float g = 0.0;
  float b = 0.0;

  if (h >= 0.0 && h < 1.0) {
    r = c;
    g = x;
    b = 0.0;
  } else if (h >= 1.0 && h < 2.0) {
    r = x;
    g = c;
    b = 0.0;
  } else if (h >= 2.0 && h < 3.0) {
    r = 0.0;
    g = c;
    b = x;
  } else if (h >= 3.0 && h < 4.0) {
    r = 0.0;
    g = x;
    b = c;
  } else if (h >= 4.0 && h < 5.0) {
    r = x;
    g = 0.0;
    b = c;
  } else {
    r = c;
    g = 0.0;
    b = x;
  }

  float4 rgba;
  rgba.x = (r + m);
  rgba.y = (g + m);
  rgba.z = (b + m);
  rgba.w = a;
  return rgba;
}

// The sRGB transfer function of IEC 61966-2-1, the one CSS Color 4 uses. Out-of-gamut components keep
// their sign, so an Oklab mix that leaves the gamut does not turn into NaN.
float3 srgb_to_linear(float3 color) {
  float3 magnitude = abs(color);
  float3 linear_part = magnitude / 12.92;
  float3 power_part = pow((magnitude + 0.055) / 1.055, float3(2.4));
  return sign(color) * select(power_part, linear_part, magnitude <= 0.04045);
}

float3 linear_to_srgb(float3 color) {
  float3 magnitude = abs(color);
  float3 linear_part = magnitude * 12.92;
  float3 power_part = 1.055 * pow(magnitude, float3(1.0 / 2.4)) - 0.055;
  return sign(color) * select(power_part, linear_part, magnitude <= 0.0031308);
}

// Converts a sRGB color to the Oklab color space.
// Reference: https://bottosson.github.io/posts/oklab/#converting-from-linear-srgb-to-oklab
float4 srgb_to_oklab(float4 color) {
  // Convert non-linear sRGB to linear sRGB
  color = float4(srgb_to_linear(color.rgb), color.a);

  float l = 0.4122214708 * color.r + 0.5363325363 * color.g + 0.0514459929 * color.b;
  float m = 0.2119034982 * color.r + 0.6806995451 * color.g + 0.1073969566 * color.b;
  float s = 0.0883024619 * color.r + 0.2817188376 * color.g + 0.6299787005 * color.b;

  float l_ = pow(l, 1.0/3.0);
  float m_ = pow(m, 1.0/3.0);
  float s_ = pow(s, 1.0/3.0);

  return float4(
   	0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_,
   	1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_,
   	0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_,
   	color.a
  );
}

// Converts an Oklab color to the sRGB color space.
float4 oklab_to_srgb(float4 color) {
  float l_ = color.r + 0.3963377774 * color.g + 0.2158037573 * color.b;
  float m_ = color.r - 0.1055613458 * color.g - 0.0638541728 * color.b;
  float s_ = color.r - 0.0894841775 * color.g - 1.2914855480 * color.b;

  float l = l_ * l_ * l_;
  float m = m_ * m_ * m_;
  float s = s_ * s_ * s_;

  float3 linear_rgb = float3(
   	4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
   	-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
   	-0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s
  );

  // Convert linear sRGB to non-linear sRGB
  return float4(linear_to_srgb(linear_rgb), color.a);
}

float4 to_device_position(float2 unit_vertex, Bounds_ScaledPixels bounds,
                          constant Size_DevicePixels *input_viewport_size) {
  float2 position =
      unit_vertex * float2(bounds.size.width, bounds.size.height) +
      float2(bounds.origin.x, bounds.origin.y);
  float2 viewport_size = float2((float)input_viewport_size->width,
                                (float)input_viewport_size->height);
  float2 device_position =
      position / viewport_size * float2(2., -2.) + float2(-1., 1.);
  return float4(device_position, 0., 1.);
}

float4 to_device_position_transformed(float2 unit_vertex, Bounds_ScaledPixels bounds,
                          TransformationMatrix transformation,
                          constant Size_DevicePixels *input_viewport_size) {
  float2 position =
      unit_vertex * float2(bounds.size.width, bounds.size.height) +
      float2(bounds.origin.x, bounds.origin.y);

  // Apply the transformation matrix to the position via matrix multiplication.
  float2 transformed_position = float2(0, 0);
  transformed_position[0] = position[0] * transformation.rotation_scale[0][0] + position[1] * transformation.rotation_scale[0][1];
  transformed_position[1] = position[0] * transformation.rotation_scale[1][0] + position[1] * transformation.rotation_scale[1][1];

  // Add in the translation component of the transformation matrix.
  transformed_position[0] += transformation.translation[0];
  transformed_position[1] += transformation.translation[1];

  float2 viewport_size = float2((float)input_viewport_size->width,
                                (float)input_viewport_size->height);
  float2 device_position =
      transformed_position / viewport_size * float2(2., -2.) + float2(-1., 1.);
  return float4(device_position, 0., 1.);
}


float2 to_tile_position(float2 unit_vertex, AtlasTile tile,
                        constant Size_DevicePixels *atlas_size) {
  float2 tile_origin = float2(tile.bounds.origin.x, tile.bounds.origin.y);
  float2 tile_size = float2(tile.bounds.size.width, tile.bounds.size.height);
  return (tile_origin + unit_vertex * tile_size) /
         float2((float)atlas_size->width, (float)atlas_size->height);
}

// The horizontal and vertical radius of a corner. A zero horizontal radius
// makes the corner square, and a zero vertical one stands for the horizontal.
float2 corner_radius_pair(float horizontal, float vertical) {
  if (horizontal == 0.0) {
    return float2(0.0);
  }
  return float2(horizontal, vertical == 0.0 ? horizontal : vertical);
}

// Selects the corner radii based on quadrant.
float2 pick_corner_radius(float2 center_to_point, Corners_ScaledPixels corner_radii,
                          Corners_ScaledPixels corner_radii_y) {
  if (center_to_point.x < 0.) {
    if (center_to_point.y < 0.) {
      return corner_radius_pair(corner_radii.top_left, corner_radii_y.top_left);
    } else {
      return corner_radius_pair(corner_radii.bottom_left, corner_radii_y.bottom_left);
    }
  } else {
    if (center_to_point.y < 0.) {
      return corner_radius_pair(corner_radii.top_right, corner_radii_y.top_right);
    } else {
      return corner_radius_pair(corner_radii.bottom_right, corner_radii_y.bottom_right);
    }
  }
}

// Signed distance of the point to the quad's border - positive outside the
// border, and negative inside.
float quad_sdf(float2 point, Bounds_ScaledPixels bounds,
               Corners_ScaledPixels corner_radii,
               Corners_ScaledPixels corner_radii_y) {
    float2 half_size = float2(bounds.size.width, bounds.size.height) / 2.0;
    float2 center = float2(bounds.origin.x, bounds.origin.y) + half_size;
    float2 center_to_point = point - center;
    float2 corner_radius = pick_corner_radius(center_to_point, corner_radii, corner_radii_y);
    float2 corner_to_point = fabs(center_to_point) - half_size;
    float2 corner_center_to_point = corner_to_point + corner_radius;
    return quad_sdf_impl(corner_center_to_point, corner_radius);
}

// Implementation of quad signed distance field
float quad_sdf_impl(float2 corner_center_to_point, float2 corner_radius) {
    if (corner_radius.x == 0.0) {
        // Fast path for unrounded corners
        return max(corner_center_to_point.x, corner_center_to_point.y);
    } else if (corner_radius.x == corner_radius.y) {
        // Signed distance of the point from a quad that is inset by corner_radius
        // It is negative inside this quad, and positive outside
        float signed_distance_to_inset_quad =
            // 0 inside the inset quad, and positive outside
            length(max(float2(0.0), corner_center_to_point)) +
            // 0 outside the inset quad, and negative inside
            min(0.0, max(corner_center_to_point.x, corner_center_to_point.y));

        return signed_distance_to_inset_quad - corner_radius.x;
    } else if (corner_center_to_point.x <= 0.0 || corner_center_to_point.y <= 0.0) {
        // Along a straight edge, beside the elliptical arc
        return max(corner_center_to_point.x - corner_radius.x,
                   corner_center_to_point.y - corner_radius.y);
    } else {
        return ellipse_corner_sdf(corner_center_to_point, corner_radius);
    }
}

// A standard gaussian function, used for weighting samples
float gaussian(float x, float sigma) {
  return exp(-(x * x) / (2. * sigma * sigma)) / (sqrt(2. * M_PI_F) * sigma);
}

// This approximates the error function, needed for the gaussian integral
float2 erf(float2 x) {
  float2 s = sign(x);
  float2 a = abs(x);
  float2 r1 = 1. + (0.278393 + (0.230389 + (0.000972 + 0.078108 * a) * a) * a) * a;
  float2 r2 = r1 * r1;
  return s - s / (r2 * r2);
}

float blur_along_x(float x, float y, float sigma, float2 corner,
                   float2 half_size) {
  float delta = min(half_size.y - corner.y - abs(y), 0.);
  float arc = corner.x == corner.y
      ? sqrt(max(0., corner.x * corner.x - delta * delta))
      : corner.x * sqrt(max(0., 1. - (delta * delta) / (corner.y * corner.y)));
  float curved = half_size.x - corner.x + arc;
  float2 integral =
      0.5 + 0.5 * erf((x + float2(-curved, curved)) * (sqrt(0.5) / sigma));
  return integral.y - integral.x;
}

float4 distance_from_clip_rect(float2 unit_vertex, Bounds_ScaledPixels bounds,
                               Bounds_ScaledPixels clip_bounds) {
  float2 position =
      unit_vertex * float2(bounds.size.width, bounds.size.height) +
      float2(bounds.origin.x, bounds.origin.y);
  return float4(position.x - clip_bounds.origin.x,
                clip_bounds.origin.x + clip_bounds.size.width - position.x,
                position.y - clip_bounds.origin.y,
                clip_bounds.origin.y + clip_bounds.size.height - position.y);
}

float4 distance_from_clip_rect_transformed(float2 unit_vertex, Bounds_ScaledPixels bounds,
                               Bounds_ScaledPixels clip_bounds, TransformationMatrix transformation) {
  float2 position =
      unit_vertex * float2(bounds.size.width, bounds.size.height) +
      float2(bounds.origin.x, bounds.origin.y);
  float2 transformed_position = float2(0, 0);
  transformed_position[0] = position[0] * transformation.rotation_scale[0][0] + position[1] * transformation.rotation_scale[0][1];
  transformed_position[1] = position[0] * transformation.rotation_scale[1][0] + position[1] * transformation.rotation_scale[1][1];
  transformed_position[0] += transformation.translation[0];
  transformed_position[1] += transformation.translation[1];

  return float4(transformed_position.x - clip_bounds.origin.x,
                clip_bounds.origin.x + clip_bounds.size.width - transformed_position.x,
                transformed_position.y - clip_bounds.origin.y,
                clip_bounds.origin.y + clip_bounds.size.height - transformed_position.y);
}

bool is_identity_transform(TransformationMatrix transformation) {
  return transformation.rotation_scale[0][0] == 1.0 &&
         transformation.rotation_scale[0][1] == 0.0 &&
         transformation.rotation_scale[1][0] == 0.0 &&
         transformation.rotation_scale[1][1] == 1.0 &&
         transformation.translation[0] == 0.0 &&
         transformation.translation[1] == 0.0;
}

// The device pixels a unit of the primitive's own space covers once the matrix
// is applied, as the square root of the area scale.
float transform_pixel_scale(TransformationMatrix transformation) {
  float determinant =
      transformation.rotation_scale[0][0] * transformation.rotation_scale[1][1] -
      transformation.rotation_scale[0][1] * transformation.rotation_scale[1][0];
  return sqrt(fabs(determinant));
}

float2 apply_transform(TransformationMatrix transformation, float2 position) {
  return float2(
      position.x * transformation.rotation_scale[0][0] +
          position.y * transformation.rotation_scale[0][1] +
          transformation.translation[0],
      position.x * transformation.rotation_scale[1][0] +
          position.y * transformation.rotation_scale[1][1] +
          transformation.translation[1]);
}

float4 device_position_of(float2 position,
                          constant Size_DevicePixels *input_viewport_size) {
  float2 viewport_size = float2((float)input_viewport_size->width,
                                (float)input_viewport_size->height);
  return float4(position / viewport_size * float2(2., -2.) + float2(-1., 1.), 0., 1.);
}

float4 distance_from_clip(float2 position, Bounds_ScaledPixels clip_bounds) {
  return float4(position.x - clip_bounds.origin.x,
                clip_bounds.origin.x + clip_bounds.size.width - position.x,
                position.y - clip_bounds.origin.y,
                clip_bounds.origin.y + clip_bounds.size.height - position.y);
}

// Coverage of the point by a clip in the primitive's own space, rectangle
// included: unlike the window-space mask, no clip distance enforces it.
float local_clip_coverage(float2 local_position, ContentMask_ScaledPixels local_clip,
                          float pixel) {
  return saturate(0.5 - quad_sdf(local_position, local_clip.bounds,
                                 local_clip.corner_radii,
                                 local_clip.corner_radii_y) * pixel);
}

float rect_coverage(float2 local_position, Bounds_ScaledPixels bounds, float pixel) {
  float2 half_size = float2(bounds.size.width, bounds.size.height) / 2.0;
  float2 center = float2(bounds.origin.x, bounds.origin.y) + half_size;
  float2 outside = fabs(local_position - center) - half_size;
  float distance = length(max(outside, float2(0.0))) + min(max(outside.x, outside.y), 0.0);
  return saturate(0.5 - distance * pixel);
}

// The vertex of a primitive whose own space is `transformation` away from the window. Without
// a matrix it is the plain quad of `bounds`. With one, the quad grows by a device pixel on
// every side, where the antialiased edges are drawn, and goes through the matrix; the clip
// distances are those of the window position.
TransformedVertex transformed_vertex(float2 unit_vertex, Bounds_ScaledPixels bounds,
                                     TransformationMatrix transformation,
                                     Bounds_ScaledPixels clip_bounds,
                                     constant Size_DevicePixels *viewport_size) {
  TransformedVertex result;
  if (is_identity_transform(transformation)) {
    result.device_position = to_device_position(unit_vertex, bounds, viewport_size);
    result.clip_distance = distance_from_clip_rect(unit_vertex, bounds, clip_bounds);
    result.unit_position = unit_vertex;
    result.local_position = float2(0.0);
    result.local_scale = 0.0;
    return result;
  }
  float local_scale = transform_pixel_scale(transformation);
  if (local_scale < 1e-6) {
    // A matrix that flattens the plane draws nothing: put the quad off the screen.
    result.device_position = float4(2.0, 2.0, 0.0, 1.0);
    result.clip_distance = float4(-1.0);
    result.unit_position = float2(0.0);
    result.local_position = float2(0.0);
    result.local_scale = 1.0;
    return result;
  }
  float pad = 1.0 / local_scale;
  float2 size = float2(bounds.size.width, bounds.size.height);
  float2 origin = float2(bounds.origin.x, bounds.origin.y) - pad;
  float2 extent = size + 2.0 * pad;
  float2 local_position = origin + unit_vertex * extent;
  float2 window_position = apply_transform(transformation, local_position);
  result.device_position = device_position_of(window_position, viewport_size);
  result.clip_distance = distance_from_clip(window_position, clip_bounds);
  result.unit_position = (local_position - float2(bounds.origin.x, bounds.origin.y)) / max(size, float2(1e-6));
  result.local_position = local_position;
  result.local_scale = local_scale;
  return result;
}

float4 over(float4 below, float4 above) {
  float4 result;
  float alpha = above.a + below.a * (1.0 - above.a);
  result.rgb =
      (above.rgb * above.a + below.rgb * below.a * (1.0 - above.a)) / alpha;
  result.a = alpha;
  return result;
}

GradientColor prepare_fill_color(uint tag, uint color_space, Hsla solid,
                                     Hsla color0, Hsla color1) {
  GradientColor out;
  if (tag == 0 || tag == 2 || tag == 3) {
    out.solid = hsla_to_rgba(solid);
  } else if (tag == 1) {
    out.color0 = hsla_to_rgba(color0);
    out.color1 = hsla_to_rgba(color1);

    // Prepare color space in vertex for avoid conversion
    // in fragment shader for performance reasons
    if (color_space == 1) {
      // Oklab
      out.color0 = srgb_to_oklab(out.color0);
      out.color1 = srgb_to_oklab(out.color1);
    }
  }

  return out;
}

float2x2 rotate2d(float angle) {
    float s = sin(angle);
    float c = cos(angle);
    return float2x2(c, -s, s, c);
}

float4 fill_color(Background background,
                      float2 position,
                      Bounds_ScaledPixels bounds,
                      float4 solid_color, float4 color0, float4 color1) {
  float4 color;

  switch (background.tag) {
    case 0:
      color = solid_color;
      break;
    case 1: {
      // -90 degrees to match the CSS gradient angle.
      float gradient_angle = background.gradient_angle_or_pattern_height;
      float radians = (fmod(gradient_angle, 360.0) - 90.0) * (M_PI_F / 180.0);
      float2 direction = float2(cos(radians), sin(radians));

      // Expand the short side to be the same as the long side
      if (bounds.size.width > bounds.size.height) {
          direction.y *= bounds.size.height / bounds.size.width;
      } else {
          direction.x *=  bounds.size.width / bounds.size.height;
      }

      // Get the t value for the linear gradient with the color stop percentages.
      float2 half_size = float2(bounds.size.width, bounds.size.height) / 2.;
      float2 center = float2(bounds.origin.x, bounds.origin.y) + half_size;
      float2 center_to_point = position - center;
      float t = dot(center_to_point, direction) / length(direction);
      // Check the direction to determine whether to use x or y
      if (abs(direction.x) > abs(direction.y)) {
          t = (t + half_size.x) / bounds.size.width;
      } else {
          t = (t + half_size.y) / bounds.size.height;
      }

      // Adjust t based on the stop percentages
      t = (t - background.colors[0].percentage)
        / (background.colors[1].percentage
        - background.colors[0].percentage);
      t = clamp(t, 0.0, 1.0);

      switch (background.color_space) {
        case 0:
          color = mix(color0, color1, t);
          break;
        case 1: {
          float4 oklab_color = mix(color0, color1, t);
          color = oklab_to_srgb(oklab_color);
          break;
        }
      }

      // Dither to reduce banding in gradients (especially dark/alpha).
      // Triangular-distributed noise breaks up 8-bit quantization steps.
      // ±2/255 for RGB (enough for dark-on-dark compositing),
      // ±3/255 for alpha (needs more because alpha × dark color = tiny steps).
      {
        float2 seed = position * 0.6180339887; // golden ratio spread
        float r1 = fract(sin(dot(seed, float2(12.9898, 78.233))) * 43758.5453);
        float r2 = fract(sin(dot(seed, float2(39.3460, 11.135))) * 24634.6345);
        float tri = r1 + r2 - 1.0; // triangular PDF, range [-1, +1]
        color.rgb += tri * 2.0 / 255.0;
        color.a   += tri * 3.0 / 255.0;
      }

      break;
    }
    case 2: {
        float gradient_angle_or_pattern_height = background.gradient_angle_or_pattern_height;
        float pattern_width = (gradient_angle_or_pattern_height / 65535.0f) / 255.0f;
        float pattern_interval = fmod(gradient_angle_or_pattern_height, 65535.0f) / 255.0f;
        float pattern_height = pattern_width + pattern_interval;
        float stripe_angle = M_PI_F / 4.0;
        float pattern_period = pattern_height * sin(stripe_angle);
        float2x2 rotation = rotate2d(stripe_angle);
        float2 relative_position = position - float2(bounds.origin.x, bounds.origin.y);
        float2 rotated_point = rotation * relative_position;
        float pattern = fmod(rotated_point.x, pattern_period);
        float distance = min(pattern, pattern_period - pattern) - pattern_period * (pattern_width / pattern_height) /  2.0f;
        color = solid_color;
        color.a *= saturate(0.5 - distance);
        break;
    }
    case 4:
      color = css_gradient_color(background, position, bounds);
      break;
    case 3: {
        // checkerboard
        float size = background.gradient_angle_or_pattern_height;
        float2 relative_position = position - float2(bounds.origin.x, bounds.origin.y);

        float x_index = floor(relative_position.x / size);
        float y_index = floor(relative_position.y / size);
        float should_be_colored = fmod(x_index + y_index, 2.0);

        color = solid_color;
        color.a *= saturate(should_be_colored);
        break;
    }
  }

  return color;
}

// Triangular-distributed noise that breaks up 8-bit quantization steps in gradients. It leaves alpha
// alone: noise there would let the backdrop show through an opaque gradient.
float4 dither(float4 color, float2 position) {
  float2 seed = position * 0.6180339887; // golden ratio spread
  float r1 = fract(sin(dot(seed, float2(12.9898, 78.233))) * 43758.5453);
  float r2 = fract(sin(dot(seed, float2(39.3460, 11.135))) * 24634.6345);
  float tri = r1 + r2 - 1.0; // triangular PDF, range [-1, +1]
  color.rgb += tri * 2.0 / 255.0;
  return color;
}

// A stop's colour as it is interpolated: premultiplied, in the gradient's colour space.
float4 css_gradient_stop(Hsla stop, uint color_space) {
  float4 color = hsla_to_rgba(stop);
  if (color_space == 1) {
    color = srgb_to_oklab(color);
  }
  return float4(color.rgb * color.a, color.a);
}

// CSS linear, radial and conic gradients, one chunk of at most four stops.
// `bounds` is the tile the gradient's geometry is relative to.
float4 css_gradient_color(Background background, float2 position, Bounds_ScaledPixels bounds) {
  GradientParams g = background.gradient;
  float2 local = position - float2(bounds.origin.x, bounds.origin.y);

  float t;
  if (g.kind == 0) {
    float2 start = float2(g.geometry.a, g.geometry.b);
    float2 line = float2(g.geometry.c, g.geometry.d) - start;
    t = dot(local - start, line) / dot(line, line);
  } else if (g.kind == 1) {
    float2 radii = float2(g.geometry.c, g.geometry.d);
    t = length((local - float2(g.geometry.a, g.geometry.b)) / radii);
  } else {
    float2 offset = local - float2(g.geometry.a, g.geometry.b);
    // Clockwise from the top, in turns.
    t = fract(atan2(offset.x, -offset.y) / (2.0 * M_PI_F) - g.geometry.c);
  }

  if (g.repeating != 0u) {
    t = g.repeat_start + (t - g.repeat_start) - g.repeat_length * floor((t - g.repeat_start) / g.repeat_length);
  }
  if (t < g.t_min || t >= g.t_max) {
    return float4(0.0);
  }

  float stop_positions[4] = {
    background.colors[0].percentage, background.colors[1].percentage,
    g.extra_colors[0].percentage, g.extra_colors[1].percentage};
  float4 stop_colors[4] = {
    css_gradient_stop(background.colors[0].color, background.color_space),
    css_gradient_stop(background.colors[1].color, background.color_space),
    css_gradient_stop(g.extra_colors[0].color, background.color_space),
    css_gradient_stop(g.extra_colors[1].color, background.color_space)};

  float4 color = stop_colors[0];
  for (uint i = 0; i + 1 < g.stop_count; i++) {
    if (t >= stop_positions[i]) {
      // Later segments override earlier ones, which makes a hard stop a jump.
      color = t < stop_positions[i + 1]
        ? mix(stop_colors[i], stop_colors[i + 1],
              (t - stop_positions[i]) / (stop_positions[i + 1] - stop_positions[i]))
        : stop_colors[i + 1];
    }
  }

  color = color.a > 0.0 ? float4(color.rgb / color.a, color.a) : float4(0.0);
  if (background.color_space == 1) {
    color = oklab_to_srgb(color);
  }
  return dither(color, position);
}

// ---------------------------------------------------------------------------
// Offscreen effect groups: compositing, filters, blur
// ---------------------------------------------------------------------------

struct EffectVertexOutput {
  float4 position [[position]];
};

// Everything the composite pass needs. All the vectors are float4 so the layout is the same on
// both sides without padding.
struct CompositeParams {
  // Where the source is drawn in the target: x, y, width, height.
  float4 dest;
  // Where the source texture's first texel lands in the target (x, y), then 1 / texture size.
  float4 source;
  // The same for the first and the second mask texture.
  float4 mask_a;
  float4 mask_b;
  // The content mask: x, y, width, height, then its horizontal and vertical corner radii (top left,
  // top right, bottom right, bottom left each). A zero vertical radius is the horizontal one.
  float4 content_mask;
  float4 content_radii;
  float4 content_radii_y;
  // The clip path in group space. A rounded rectangle is x, y, width, height and radii; an ellipse
  // is the centre and radii. Polygon points come in a buffer.
  float4 clip_a;
  float4 clip_radii;
  float4 clip_radii_y;
  // The target position of group space's origin, the clip kind (0 none, 1 rounded rectangle,
  // 2 ellipse, 3 polygon) and the polygon's vertex count.
  float4 group;
  // Opacity, even-odd fill, mask A present, mask B present.
  float4 misc;
  // A rounded rectangle in target space the result is also clipped to, when it has a width.
  float4 reference;
  float4 reference_radii;
  float4 reference_radii_y;
  // When the reference is in the element's own space (offset.w is 1): the matrix from the
  // target to that space (a, b, c, d of its rows) and its translation (offset.xy), and the
  // device pixels one unit of that space covers (offset.z).
  float4 reference_matrix;
  float4 reference_offset;
};

float rounded_rect_sdf(float2 position, float4 rect, float4 radii, float4 radii_y) {
  float2 half_size = rect.zw / 2.0;
  float2 center = rect.xy + half_size;
  float2 offset = position - center;
  float2 radius = corner_radius_pair(
    offset.x < 0.0
      ? (offset.y < 0.0 ? radii.x : radii.w)
      : (offset.y < 0.0 ? radii.y : radii.z),
    offset.x < 0.0
      ? (offset.y < 0.0 ? radii_y.x : radii_y.w)
      : (offset.y < 0.0 ? radii_y.y : radii_y.z));
  float2 corner = fabs(offset) - half_size + radius;
  if (radius.x == radius.y) {
    return length(max(corner, float2(0.0))) + min(max(corner.x, corner.y), 0.0) - radius.x;
  }
  if (corner.x <= 0.0 || corner.y <= 0.0) {
    return max(corner.x - radius.x, corner.y - radius.y);
  }
  return ellipse_corner_sdf(corner, radius);
}

// Distance to an ellipse, good enough to antialias its edge.
float ellipse_sdf(float2 position, float2 center, float2 radii) {
  float2 offset = position - center;
  float k0 = length(offset / radii);
  float k1 = length(offset / (radii * radii));
  return k1 > 0.0 ? k0 * (k0 - 1.0) / k1 : -min(radii.x, radii.y);
}

// A point whose x is above this separates two contours, each closed on its own. An edge between
// them would be a seam of half coverage inside the shape.
constant float CONTOUR_BREAK = 1e37;

// Signed distance to a polygon: negative inside.
float polygon_sdf(float2 position, constant float2 *points, uint count, bool even_odd) {
  float min_distance_squared = 1e20;
  int winding = 0;
  uint crossings = 0;
  uint contour_start = 0;
  for (uint i = 0; i < count; i++) {
    float2 start = points[i];
    if (start.x > CONTOUR_BREAK) {
      contour_start = i + 1;
      continue;
    }
    bool last_of_contour = i + 1 == count || points[i + 1].x > CONTOUR_BREAK;
    float2 end = last_of_contour ? points[contour_start] : points[i + 1];
    float2 edge = end - start;
    float2 to_point = position - start;
    float along = clamp(dot(to_point, edge) / max(dot(edge, edge), 1e-12), 0.0, 1.0);
    float2 nearest = to_point - edge * along;
    min_distance_squared = min(min_distance_squared, dot(nearest, nearest));

    float side = edge.x * to_point.y - to_point.x * edge.y;
    if (start.y <= position.y) {
      if (end.y > position.y && side > 0.0) {
        winding++;
        crossings++;
      }
    } else if (end.y <= position.y && side < 0.0) {
      winding--;
      crossings++;
    }
  }
  bool inside = even_odd ? (crossings % 2 == 1) : (winding != 0);
  float distance = sqrt(min_distance_squared);
  return inside ? -distance : distance;
}

vertex EffectVertexOutput composite_vertex(
    uint unit_vertex_id [[vertex_id]],
    constant float2 *unit_vertices [[buffer(0)]],
    constant CompositeParams &params [[buffer(1)]],
    constant Size_DevicePixels *viewport_size [[buffer(2)]]) {
  float2 position = unit_vertices[unit_vertex_id] * params.dest.zw + params.dest.xy;
  float2 viewport = float2((float)viewport_size->width, (float)viewport_size->height);
  float2 device_position = position / viewport * float2(2., -2.) + float2(-1., 1.);
  return EffectVertexOutput{float4(device_position, 0., 1.)};
}

fragment float4 composite_fragment(
    EffectVertexOutput input [[stage_in]],
    constant CompositeParams &params [[buffer(1)]],
    constant float2 *polygon [[buffer(3)]],
    texture2d<float> source [[texture(0)]],
    texture2d<float> mask_a [[texture(1)]],
    texture2d<float> mask_b [[texture(2)]]) {
  constexpr sampler linear(mag_filter::linear, min_filter::linear, address::clamp_to_zero);
  float2 position = input.position.xy;

  // Premultiplied.
  float4 color = source.sample(linear, (position - params.source.xy) * params.source.zw);

  float coverage = saturate(0.5 - rounded_rect_sdf(position, params.content_mask, params.content_radii, params.content_radii_y));

  if (params.reference.z > 0.0) {
    if (params.reference_offset.w > 0.5) {
      float2 local_reference = float2(
          params.reference_matrix.x * position.x + params.reference_matrix.y * position.y + params.reference_offset.x,
          params.reference_matrix.z * position.x + params.reference_matrix.w * position.y + params.reference_offset.y);
      coverage *= saturate(0.5 - rounded_rect_sdf(local_reference, params.reference, params.reference_radii, params.reference_radii_y) * params.reference_offset.z);
    } else {
      coverage *= saturate(0.5 - rounded_rect_sdf(position, params.reference, params.reference_radii, params.reference_radii_y));
    }
  }

  float2 local = position - params.group.xy;
  uint clip_kind = (uint)params.group.z;
  if (clip_kind == 1) {
    coverage *= saturate(0.5 - rounded_rect_sdf(local, params.clip_a, params.clip_radii, params.clip_radii_y));
  } else if (clip_kind == 2) {
    coverage *= saturate(0.5 - ellipse_sdf(local, params.clip_a.xy, params.clip_a.zw));
  } else if (clip_kind == 3) {
    coverage *= saturate(0.5 - polygon_sdf(local, polygon, (uint)params.group.w, params.misc.y > 0.5));
  }

  if (params.misc.z > 0.5) {
    coverage *= mask_a.sample(linear, (position - params.mask_a.xy) * params.mask_a.zw).a;
  }
  if (params.misc.w > 0.5) {
    coverage *= mask_b.sample(linear, (position - params.mask_b.xy) * params.mask_b.zw).a;
  }

  return color * (coverage * params.misc.x);
}

// A pass over the whole target.
vertex EffectVertexOutput fullscreen_vertex(
    uint unit_vertex_id [[vertex_id]],
    constant float2 *unit_vertices [[buffer(0)]]) {
  float2 unit = unit_vertices[unit_vertex_id];
  return EffectVertexOutput{float4(unit * float2(2., -2.) + float2(-1., 1.), 0., 1.)};
}

struct FilterColorParams {
  uint matrix_count;
};

// Runs of CSS colour filters. Each matrix is five float4s: the weights of the red, green, blue
// and alpha outputs on (r, g, b, a), then the offsets.
fragment float4 filter_color_fragment(
    EffectVertexOutput input [[stage_in]],
    constant FilterColorParams &params [[buffer(0)]],
    constant float4 *matrices [[buffer(1)]],
    texture2d<float> source [[texture(0)]]) {
  float4 premultiplied = source.read(uint2(input.position.xy));
  float4 color = premultiplied.a > 0.0
    ? float4(premultiplied.rgb / premultiplied.a, premultiplied.a)
    : float4(0.0);
  for (uint i = 0; i < params.matrix_count; i++) {
    constant float4 *matrix = matrices + i * 5;
    float4 result = float4(
      dot(matrix[0], color) + matrix[4].x,
      dot(matrix[1], color) + matrix[4].y,
      dot(matrix[2], color) + matrix[4].z,
      dot(matrix[3], color) + matrix[4].w);
    color = saturate(result);
  }
  return float4(color.rgb * color.a, color.a);
}

struct BlurParams {
  // One texel along the blur axis, in uv units.
  float2 step;
  float sigma;
  int radius;
  // Nonzero to repeat the edge pixels instead of reading transparency past them.
  uint clamp_to_edge;
  // The part of the source that holds the image, in uv units; the rest is padding. The edge pixels
  // repeat at the end of this part, not of the texture.
  float2 valid;
};

fragment float4 blur_fragment(
    EffectVertexOutput input [[stage_in]],
    constant BlurParams &params [[buffer(0)]],
    texture2d<float> source [[texture(0)]]) {
  constexpr sampler zero(mag_filter::linear, min_filter::linear, address::clamp_to_zero);
  constexpr sampler edge(mag_filter::linear, min_filter::linear, address::clamp_to_edge);
  float2 texel = 1.0 / float2(source.get_width(), source.get_height());
  float2 uv = input.position.xy * texel;
  float4 sum = float4(0.0);
  float total = 0.0;
  for (int i = -params.radius; i <= params.radius; i++) {
    float weight = exp(-(float)(i * i) / (2.0 * params.sigma * params.sigma));
    float2 at = uv + params.step * (float)i;
    if (params.clamp_to_edge != 0u) {
      sum += weight * source.sample(edge, clamp(at, 0.5 * texel, params.valid - 0.5 * texel));
    } else {
      sum += weight * source.sample(zero, at);
    }
    total += weight;
  }
  return sum / total;
}

// Copies the source scaled to the size of the target, filtering bilinearly.
fragment float4 resample_fragment(
    EffectVertexOutput input [[stage_in]],
    constant float2 &target_size [[buffer(0)]],
    texture2d<float> source [[texture(0)]]) {
  constexpr sampler linear(mag_filter::linear, min_filter::linear, address::clamp_to_edge);
  return source.sample(linear, input.position.xy / target_size);
}

struct DropShadowParams {
  // Where the shadow lies relative to the content, in pixels.
  float2 offset;
  float2 pad;
  // The shadow colour, premultiplied.
  float4 color;
};

// The content over its own blurred, offset, tinted alpha.
fragment float4 drop_shadow_fragment(
    EffectVertexOutput input [[stage_in]],
    constant DropShadowParams &params [[buffer(0)]],
    texture2d<float> content [[texture(0)]],
    texture2d<float> blurred [[texture(1)]]) {
  constexpr sampler linear(mag_filter::linear, min_filter::linear, address::clamp_to_zero);
  float4 base = content.read(uint2(input.position.xy));
  float2 size = float2(blurred.get_width(), blurred.get_height());
  float shadow_alpha = blurred.sample(linear, (input.position.xy - params.offset) / size).a;
  return base + params.color * shadow_alpha * (1.0 - base.a);
}

struct MaskCombineParams {
  // Nonzero to read the layer by luminance, zero by alpha.
  uint luminance;
  // 0 add, 1 subtract, 2 intersect, 3 exclude.
  uint op;
  // Zero for the bottom layer, which has nothing below it.
  uint has_below;
  uint pad;
};

// One layer of a mask over the mask below it, as `mask-mode` and `mask-composite` say. The result
// is the new mask, written to every channel; the composite reads its alpha.
fragment float4 mask_combine_fragment(
    EffectVertexOutput input [[stage_in]],
    constant MaskCombineParams &params [[buffer(0)]],
    texture2d<float> layer [[texture(0)]],
    texture2d<float> below [[texture(1)]]) {
  uint2 texel = uint2(input.position.xy);
  float4 premultiplied = layer.read(texel);
  // The layer is premultiplied, so the luminance of its colour is already scaled by its alpha.
  float source = params.luminance != 0u
    ? dot(premultiplied.rgb, float3(0.2125, 0.7154, 0.0721))
    : premultiplied.a;
  float destination = params.has_below != 0u ? below.read(texel).a : 0.0;
  float result;
  if (params.op == 1u) {
    result = source * (1.0 - destination);
  } else if (params.op == 2u) {
    result = source * destination;
  } else if (params.op == 3u) {
    result = source * (1.0 - destination) + destination * (1.0 - source);
  } else {
    result = source + destination * (1.0 - source);
  }
  return float4(saturate(result));
}
