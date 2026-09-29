/* StrataPDF shim over the PyMuPDF Layout region features (features.c).
 * Copies the features the layout model reads from C into a float array, in
 * the model's order (rf_names of layout_rf2.4.1+imf1.yaml, minus the six
 * computed in Rust: num_ratio, is_text, is_vector, is_image, is_hline_vector,
 * is_vline_vector). Licensed under AGPL-3.0-or-later like StrataPDF. */
#include "features_decls.h"

/* Generated from the rf_names of layout_rf2.4.1+imf1.yaml: the C-computed features, in model order. */
#define STRATA_RF_C_COUNT 112
static void strata_rf_copy(const fz_feature_stats *s, float *out)
{
	out[0] = (float)s->alignment_down_with_centre;
	out[1] = (float)s->alignment_down_with_left;
	out[2] = (float)s->alignment_down_with_right;
	out[3] = (float)s->alignment_left_with_baseline;
	out[4] = (float)s->alignment_left_with_bottom;
	out[5] = (float)s->alignment_left_with_middle;
	out[6] = (float)s->alignment_left_with_top;
	out[7] = (float)s->alignment_right_with_baseline;
	out[8] = (float)s->alignment_right_with_bottom;
	out[9] = (float)s->alignment_right_with_middle;
	out[10] = (float)s->alignment_right_with_top;
	out[11] = (float)s->alignment_up_with_centre;
	out[12] = (float)s->alignment_up_with_left;
	out[13] = (float)s->alignment_up_with_right;
	out[14] = (float)s->bottom_right_x;
	out[15] = (float)s->bottommost_baseline;
	out[16] = (float)s->centre;
	out[17] = (float)s->char_area;
	out[18] = (float)s->char_space;
	out[19] = (float)s->consecutive_baseline_alignment_count_left;
	out[20] = (float)s->consecutive_baseline_alignment_count_right;
	out[21] = (float)s->consecutive_bottom_alignment_count_left;
	out[22] = (float)s->consecutive_bottom_alignment_count_right;
	out[23] = (float)s->consecutive_centre_alignment_count_down;
	out[24] = (float)s->consecutive_centre_alignment_count_up;
	out[25] = (float)s->consecutive_left_alignment_count_down;
	out[26] = (float)s->consecutive_left_alignment_count_up;
	out[27] = (float)s->consecutive_middle_alignment_count_left;
	out[28] = (float)s->consecutive_middle_alignment_count_right;
	out[29] = (float)s->consecutive_right_alignment_count_down;
	out[30] = (float)s->consecutive_right_alignment_count_up;
	out[31] = (float)s->consecutive_top_alignment_count_left;
	out[32] = (float)s->consecutive_top_alignment_count_right;
	out[33] = (float)s->contains_image;
	out[34] = (float)s->contains_vector;
	out[35] = (float)s->context_above_font_size;
	out[36] = (float)s->context_above_indent;
	out[37] = (float)s->context_above_is_header;
	out[38] = (float)s->context_above_outdent;
	out[39] = (float)s->context_below_bullet;
	out[40] = (float)s->context_below_font_size;
	out[41] = (float)s->context_below_indent;
	out[42] = (float)s->context_below_is_header;
	out[43] = (float)s->context_below_outdent;
	out[44] = (float)s->context_header_differs;
	out[45] = (float)s->dodgy_paragraph_breaks;
	out[46] = (float)s->font_size;
	out[47] = (float)s->font_size_median;
	out[48] = (float)s->font_size_mode;
	out[49] = (float)s->fonts_offset;
	out[50] = (float)s->imargin_b;
	out[51] = (float)s->imargin_l;
	out[52] = (float)s->imargin_r;
	out[53] = (float)s->imargin_t;
	out[54] = (float)s->inner_b;
	out[55] = (float)s->inner_l;
	out[56] = (float)s->inner_r;
	out[57] = (float)s->inner_t;
	out[58] = (float)s->invisible;
	out[59] = (float)s->is_header;
	out[60] = (float)s->line_bullets;
	out[61] = (float)s->line_space;
	out[62] = (float)s->line_within_block;
	out[63] = (float)s->linespaces_offset;
	out[64] = (float)s->margin_b;
	out[65] = (float)s->margin_l;
	out[66] = (float)s->margin_r;
	out[67] = (float)s->margin_t;
	out[68] = (float)s->max_non_first_left_indent;
	out[69] = (float)s->max_non_last_right_indent;
	out[70] = (float)s->middle;
	out[71] = (float)s->nearest_nonaligned_down_centre;
	out[72] = (float)s->nearest_nonaligned_down_left;
	out[73] = (float)s->nearest_nonaligned_down_right;
	out[74] = (float)s->nearest_nonaligned_left_baseline;
	out[75] = (float)s->nearest_nonaligned_left_bottom;
	out[76] = (float)s->nearest_nonaligned_left_middle;
	out[77] = (float)s->nearest_nonaligned_left_top;
	out[78] = (float)s->nearest_nonaligned_right_baseline;
	out[79] = (float)s->nearest_nonaligned_right_bottom;
	out[80] = (float)s->nearest_nonaligned_right_middle;
	out[81] = (float)s->nearest_nonaligned_right_top;
	out[82] = (float)s->nearest_nonaligned_up_centre;
	out[83] = (float)s->nearest_nonaligned_up_left;
	out[84] = (float)s->nearest_nonaligned_up_right;
	out[85] = (float)s->non_line_bullets;
	out[86] = (float)s->num_fonts_in_region;
	out[87] = (float)s->num_lines;
	out[88] = (float)s->num_lines_in_block;
	out[89] = (float)s->num_non_numerals;
	out[90] = (float)s->num_numerals;
	out[91] = (float)s->num_underlines;
	out[92] = (float)s->numeral_ratio;
	out[93] = (float)s->raft_edge_down;
	out[94] = (float)s->raft_edge_left;
	out[95] = (float)s->raft_edge_right;
	out[96] = (float)s->raft_edge_up;
	out[97] = (float)s->raft_num;
	out[98] = (float)s->ratio;
	out[99] = (float)s->ray_line_distance_down;
	out[100] = (float)s->ray_line_distance_left;
	out[101] = (float)s->ray_line_distance_right;
	out[102] = (float)s->ray_line_distance_up;
	out[103] = (float)s->segment;
	out[104] = (float)s->smargin_b;
	out[105] = (float)s->smargin_l;
	out[106] = (float)s->smargin_r;
	out[107] = (float)s->smargin_t;
	out[108] = (float)s->table_element;
	out[109] = (float)s->table_num;
	out[110] = (float)s->top_left_x;
	out[111] = (float)s->topmost_baseline;
}

int strata_rf_count(void)
{
	return STRATA_RF_C_COUNT;
}

/* Features of one region, computed as PyMuPDF Layout does (a fresh
 * fz_features per region). Returns 0 on a MuPDF error. */
int strata_region_features(fz_context *ctx, fz_stext_page *page, float x0, float y0, float x1, float y1, float *out)
{
	fz_features *features = NULL;
	int ok = 1;
	fz_var(features);
	fz_try(ctx)
	{
		fz_rect region;
		region.x0 = x0;
		region.y0 = y0;
		region.x1 = x1;
		region.y1 = y1;
		features = fz_new_page_features(ctx, page);
		strata_rf_copy(fz_features_for_region(ctx, features, region, 0), out);
	}
	fz_always(ctx)
		fz_drop_page_features(ctx, features);
	fz_catch(ctx)
		ok = 0;
	return ok;
}

