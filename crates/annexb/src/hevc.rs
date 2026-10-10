//! HEVC's NAL unit types, and as much of the VPS, SPS, PPS and slice header
//! as says what a stream is and which pictures a frame predicts from. The
//! same promise as the H.264 side: any bytes give an answer, never a panic.
//!
//! Section numbers are those of the HEVC standard (ITU-T H.265).

use super::{BLOCK, Bits, Nal, SliceType, bits_for};

pub const TRAIL_N: u8 = 0;
pub const TRAIL_R: u8 = 1;
pub const IDR_W_RADL: u8 = 19;
pub const IDR_N_LP: u8 = 20;
pub const CRA: u8 = 21;
pub const VPS: u8 = 32;
pub const SPS: u8 = 33;
pub const PPS: u8 = 34;
pub const AUD: u8 = 35;
pub const FILLER: u8 = 38;
pub const PREFIX_SEI: u8 = 39;
pub const SUFFIX_SEI: u8 = 40;

// A reference picture set lists at most the decoded picture buffer's 16
// pictures (A.4.2), and a decoder never needs more to follow a stream.
const MAX_SET: usize = 16;

/// nal_unit_type from the first of HEVC's two header bytes: 19 is an IDR,
/// 1 an ordinary frame, 33 an SPS. 63, a type HEVC leaves unspecified, for
/// an empty unit.
pub fn kind(nal: &Nal<'_>) -> u8 {
    nal.data.first().map_or(63, |b| (b >> 1) & 0x3f)
}

/// TemporalId: 0 for every picture Booth's encoders make.
pub fn temporal_id(nal: &Nal<'_>) -> u8 {
    nal.data.get(1).map_or(0, |b| (b & 7).saturating_sub(1))
}

/// Picture data: the VCL types, 0 to 31.
pub fn is_slice(nal: &Nal<'_>) -> bool {
    kind(nal) < 32
}

pub fn is_idr(nal: &Nal<'_>) -> bool {
    matches!(kind(nal), IDR_W_RADL | IDR_N_LP)
}

/// A picture a decoder can start from: IDR, CRA or BLA.
pub fn is_irap(nal: &Nal<'_>) -> bool {
    (16..=23).contains(&kind(nal))
}

fn payload<'a>(nal: &Nal<'a>) -> Bits<'a> {
    Bits::new(nal.data.get(2..).unwrap_or_default())
}

/// What an SPS says about the stream, as far as Booth cares.
///
/// As with H.264's, every value is what the stream claims: a hostile SPS
/// can claim a picture up to u32::MAX wide, and anything that sizes memory
/// from it has to bound it first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sps {
    pub sps_id: u32,
    pub vps_id: u32,
    /// general_profile_idc: 1 is Main, 2 Main 10.
    pub profile_idc: u8,
    pub high_tier: bool,
    /// general_level_idc, 30 times the level: 153 is 5.1.
    pub level_idc: u8,
    /// 1 is 4:2:0.
    pub chroma_format_idc: u32,
    pub separate_colour_plane: bool,
    /// The picture as shown, inside the conformance window.
    pub width: u32,
    pub height: u32,
    /// The picture as coded, in whole minimum coding blocks.
    pub coded_width: u32,
    pub coded_height: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    /// pic_order_cnt_lsb counts modulo 2 to the power of this.
    pub log2_max_pic_order_cnt_lsb: u32,
    /// For the highest sub-layer: the pictures a decoder must hold, the one
    /// being decoded included (sps_max_dec_pic_buffering_minus1 plus 1).
    pub max_dec_pic_buffering: u32,
    /// For the highest sub-layer: how many pictures may come out of order.
    /// Anything above 0 lets a decoder hold frames back.
    pub max_num_reorder_pics: u32,
    pub max_latency_increase_plus1: u32,
    pub log2_min_cb_size: u32,
    pub log2_ctb_size: u32,
    pub sample_adaptive_offset: bool,
    /// The sets a slice header can name by index.
    pub short_term_ref_pic_sets: Vec<RefPicSet>,
    pub long_term_ref_pics_present: bool,
    /// used_by_curr_pic_lt_sps_flag of each long-term picture the SPS
    /// lists.
    pub long_term_ref_pics_sps: Vec<bool>,
    pub temporal_mvp: bool,
    pub vui: Option<Vui>,
}

impl Sps {
    /// ChromaArrayType: 0 when there is no chroma to code, monochrome or
    /// 4:4:4 as three separate planes.
    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane {
            0
        } else {
            self.chroma_format_idc
        }
    }
}

/// The part of the VUI that says what the samples mean.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vui {
    pub full_range: Option<bool>,
    /// colour_primaries, transfer_characteristics, matrix_coeffs: 1, 1, 1
    /// is BT.709.
    pub colour: Option<(u8, u8, u8)>,
}

/// A short-term reference picture set (7.4.8): the pictures a decoder keeps,
/// as picture order count differences from the current picture, each with
/// whether the current picture may predict from it (used_by_curr_pic). The
/// rest are kept only for pictures after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefPicSet {
    /// Earlier pictures, closest first: -1 is the picture just before.
    pub before: Vec<(i32, bool)>,
    /// Later pictures, closest first, which a stream without reordering
    /// never has.
    pub after: Vec<(i32, bool)>,
}

impl RefPicSet {
    /// NumPicTotalCurr's short-term part.
    pub fn used(&self) -> usize {
        self.before
            .iter()
            .chain(&self.after)
            .filter(|(_, used)| *used)
            .count()
    }
}

pub fn parse_sps(nal: &Nal<'_>) -> Option<Sps> {
    if kind(nal) != SPS {
        return None;
    }
    let mut b = payload(nal);
    let vps_id = b.bits(4)?;
    let max_sub_layers_minus1 = b.bits(3)?;
    if max_sub_layers_minus1 > 6 {
        return None;
    }
    let _temporal_id_nesting = b.flag()?;
    let (profile_idc, high_tier, level_idc) = profile_tier_level(&mut b, max_sub_layers_minus1)?;

    let sps_id = b.ue()?;
    let chroma_format_idc = b.ue()?;
    if sps_id > 15 || chroma_format_idc > 3 {
        return None;
    }
    let separate_colour_plane = chroma_format_idc == 3 && b.flag()?;
    let coded_width = b.ue()?;
    let coded_height = b.ue()?;
    let (mut crop_x, mut crop_y) = (0u64, 0u64);
    if b.flag()? {
        // The window counts in chroma samples (7.4.3.2.1).
        let (unit_x, unit_y) = match (chroma_format_idc, separate_colour_plane) {
            (1, false) => (2, 2),
            (2, false) => (2, 1),
            _ => (1, 1),
        };
        let (left, right) = (u64::from(b.ue()?), u64::from(b.ue()?));
        let (top, bottom) = (u64::from(b.ue()?), u64::from(b.ue()?));
        crop_x = unit_x * (left + right);
        crop_y = unit_y * (top + bottom);
    }
    // 8 to 16 bits (7.4.3.2.1).
    let bit_depth_luma = b.ue()?.checked_add(8).filter(|&n| n <= 16)?;
    let bit_depth_chroma = b.ue()?.checked_add(8).filter(|&n| n <= 16)?;
    let log2_max_pic_order_cnt_lsb = b.ue()?.checked_add(4).filter(|&n| n <= 16)?;

    // Every sub-layer, or only the highest; the highest's is what counts,
    // and it comes last.
    let every_layer = b.flag()?;
    let first = if every_layer {
        0
    } else {
        max_sub_layers_minus1
    };
    let (mut max_dec_pic_buffering, mut max_num_reorder_pics, mut max_latency_increase_plus1) =
        (0, 0, 0);
    for _ in first..=max_sub_layers_minus1 {
        max_dec_pic_buffering = b.ue()?.checked_add(1)?;
        max_num_reorder_pics = b.ue()?;
        max_latency_increase_plus1 = b.ue()?;
    }

    let log2_min_cb_size = b.ue()?.checked_add(3)?;
    let log2_ctb_size = log2_min_cb_size.checked_add(b.ue()?)?;
    // Coding tree blocks are 16 to 64 samples (7.4.3.2.1).
    if !(4..=6).contains(&log2_ctb_size) {
        return None;
    }
    let _log2_min_transform_block_size = b.ue()?;
    let _log2_diff_max_min_transform_block_size = b.ue()?;
    let _max_transform_hierarchy_depth_inter = b.ue()?;
    let _max_transform_hierarchy_depth_intra = b.ue()?;
    if b.flag()? && b.flag()? {
        skip_scaling_list_data(&mut b)?;
    }
    let _amp = b.flag()?;
    let sample_adaptive_offset = b.flag()?;
    if b.flag()? {
        // PCM sample bit depths, then its block sizes and loop filter flag.
        b.bits(8)?;
        b.ue()?;
        b.ue()?;
        b.flag()?;
    }

    let set_count = b.ue()?;
    if set_count > 64 {
        return None;
    }
    let mut short_term_ref_pic_sets: Vec<RefPicSet> = Vec::new();
    for index in 0..set_count as usize {
        let set = ref_pic_set(&mut b, index, &short_term_ref_pic_sets, false)?;
        short_term_ref_pic_sets.push(set);
    }
    let long_term_ref_pics_present = b.flag()?;
    let mut long_term_ref_pics_sps = Vec::new();
    if long_term_ref_pics_present {
        let count = b.ue()?;
        if count > 32 {
            return None;
        }
        for _ in 0..count {
            let _lt_ref_pic_poc_lsb_sps = b.bits(log2_max_pic_order_cnt_lsb)?;
            long_term_ref_pics_sps.push(b.flag()?);
        }
    }
    let temporal_mvp = b.flag()?;
    let _strong_intra_smoothing = b.flag()?;
    let vui = if b.flag()? {
        Some(parse_vui(&mut b)?)
    } else {
        None
    };

    let width = u64::from(coded_width).saturating_sub(crop_x);
    let height = u64::from(coded_height).saturating_sub(crop_y);
    Some(Sps {
        sps_id,
        vps_id,
        profile_idc: profile_idc as u8,
        high_tier,
        level_idc: level_idc as u8,
        chroma_format_idc,
        separate_colour_plane,
        width: width as u32,
        height: height as u32,
        coded_width,
        coded_height,
        bit_depth_luma,
        bit_depth_chroma,
        log2_max_pic_order_cnt_lsb,
        max_dec_pic_buffering,
        max_num_reorder_pics,
        max_latency_increase_plus1,
        log2_min_cb_size,
        log2_ctb_size,
        sample_adaptive_offset,
        short_term_ref_pic_sets,
        long_term_ref_pics_present,
        long_term_ref_pics_sps,
        temporal_mvp,
        vui,
    })
}

/// The coded size (pic_width_in_luma_samples by pic_height_in_luma_samples)
/// of each SPS in an access unit, in order, and None for one this reader
/// cannot read. A decoder sizes its tables by the coded size once a slice
/// brings an SPS in, which can be in the same access unit, so this is what
/// has to be bounded before a decoder sees any of it. Each SPS is cut out
/// where [`nal_units`](super::nal_units) would cut it, but the rest of the
/// unit is only searched, never split: splitting 3 MiB of the smallest NAL
/// units takes about 2 ms.
pub fn coded_sizes(access_unit: &[u8]) -> impl Iterator<Item = Option<(u32, u32)>> + '_ {
    let mut rest = access_unit;
    std::iter::from_fn(move || {
        let unit = &rest[super::find::<4>(rest, sps_starts)? + 3..];
        let end = super::find::<3>(unit, super::ends).unwrap_or(unit.len());
        rest = &unit[end..];
        let mut data = &unit[..end];
        while let [head @ .., 0] = data {
            data = head;
        }
        Some(parse_sps(&Nal { data }).map(|sps| (sps.coded_width, sps.coded_height)))
    })
}

// nal_units starts a NAL unit at every 00 00 01, as each one ends the unit
// before it, so an SPS begins wherever 00 00 01 is followed by a header byte
// of type 33, whatever its forbidden bit.
fn sps_starts(w: &[u8], i: usize) -> u8 {
    u8::from((w[i] | w[i + 1] | (w[i + 2] ^ 1) | ((w[i + 3] & 0x7e) ^ (SPS << 1))) == 0)
}

/// nal_unit_type after the first 00 00 01 that is followed by a picture's
/// type, below 32: what the frame starts with. The unit is searched a block
/// at a time, since walking 3 MiB of the smallest NAL units one start code
/// at a time takes up to about 1 ms.
pub fn first_picture(access_unit: &[u8]) -> Option<u8> {
    let mut from = 0;
    loop {
        let at = from
            + super::find::<4>(&access_unit[from..], |w, i| {
                u8::from((w[i] | w[i + 1] | (w[i + 2] ^ 1) | (w[i + 3] & 0x40)) == 0)
            })?;
        let unit = &access_unit[at + 3..];
        // Zero bytes up to the next start code are no unit to nal_units,
        // only padding, so they are no picture either.
        if unit[0] != 0 || !super::only_zeros(unit) {
            return Some((unit[0] >> 1) & 0x3f);
        }
        from = at + 3;
    }
}

/// How many SPSs an access unit carries, the number [`coded_sizes`] gives,
/// from one pass over the bytes that reads no SPS: about 0.13 ms for 3 MiB.
/// A unit packed with SPSs can be refused before any is read.
pub fn sps_count(access_unit: &[u8]) -> usize {
    // Each block summed in a byte. Two SPSs begin at least four bytes apart,
    // since the 01 and the header byte of one are not zero, so a block holds
    // at most 64.
    let positions = access_unit.len().saturating_sub(3);
    let mut count = 0;
    let mut at = 0;
    while at + BLOCK <= positions {
        let window = &access_unit[at..at + BLOCK + 3];
        let mut here = 0u8;
        for i in 0..BLOCK {
            here += sps_starts(window, i);
        }
        count += usize::from(here);
        at += BLOCK;
    }
    let tail = &access_unit[at..];
    for i in 0..positions - at {
        count += usize::from(sps_starts(tail, i));
    }
    count
}

/// profile_tier_level(1, maxNumSubLayersMinus1) (7.3.3): the general
/// profile, tier and level, with every sub-layer's skipped.
fn profile_tier_level(b: &mut Bits<'_>, max_sub_layers_minus1: u32) -> Option<(u32, bool, u32)> {
    let _profile_space = b.bits(2)?;
    let high_tier = b.flag()?;
    let profile_idc = b.bits(5)?;
    let _compatibility_flags = b.bits(32)?;
    // Progressive, interlaced, non-packed and frame-only, then 43 bits of
    // constraints and one reserved.
    b.bits(4)?;
    b.bits(32)?;
    b.bits(12)?;
    let level_idc = b.bits(8)?;

    let layers = max_sub_layers_minus1 as usize;
    let mut present = [(false, false); 7];
    for flags in present.iter_mut().take(layers) {
        *flags = (b.flag()?, b.flag()?);
    }
    if layers > 0 {
        for _ in layers..8 {
            b.bits(2)?;
        }
    }
    for &(profile, level) in present.iter().take(layers) {
        if profile {
            // The same 88 bits as the general profile above.
            b.bits(32)?;
            b.bits(32)?;
            b.bits(24)?;
        }
        if level {
            b.bits(8)?;
        }
    }
    Some((profile_idc, high_tier, level_idc))
}

/// scaling_list_data() (7.3.4), which nothing here needs but everything
/// after it does.
fn skip_scaling_list_data(b: &mut Bits<'_>) -> Option<()> {
    for size_id in 0..4u32 {
        let matrices = if size_id == 3 { 2 } else { 6 };
        for _ in 0..matrices {
            if !b.flag()? {
                let _pred_matrix_id_delta = b.ue()?;
                continue;
            }
            if size_id > 1 {
                let _dc_coef_minus8 = b.se()?;
            }
            for _ in 0..(1u32 << (4 + (size_id << 1))).min(64) {
                let _delta_coef = b.se()?;
            }
        }
    }
    Some(())
}

/// vui_parameters() (E.2.1) as far as the colour description. What comes
/// after it (chroma siting, timing, HRD, restrictions) says nothing Booth
/// checks.
fn parse_vui(b: &mut Bits<'_>) -> Option<Vui> {
    let mut vui = Vui::default();
    if b.flag()? {
        // aspect_ratio_idc 255 is Extended_SAR, followed by the ratio.
        if b.bits(8)? == 255 {
            b.bits(16)?;
            b.bits(16)?;
        }
    }
    if b.flag()? {
        let _overscan_appropriate = b.flag()?;
    }
    if b.flag()? {
        let _video_format = b.bits(3)?;
        vui.full_range = Some(b.flag()?);
        if b.flag()? {
            vui.colour = Some((b.bits(8)? as u8, b.bits(8)? as u8, b.bits(8)? as u8));
        }
    }
    Some(vui)
}

/// st_ref_pic_set(stRpsIdx) (7.3.7), worked out as 7.4.8 says. `earlier`
/// holds the sets before this one: the SPS's own while the SPS is read, all
/// of them for the one a slice header carries (`in_slice`).
fn ref_pic_set(
    b: &mut Bits<'_>,
    index: usize,
    earlier: &[RefPicSet],
    in_slice: bool,
) -> Option<RefPicSet> {
    let predicted = index != 0 && b.flag()?;
    let mut set = RefPicSet::default();
    if predicted {
        let delta_idx = if in_slice {
            (b.ue()? as usize).checked_add(1)?
        } else {
            1
        };
        let reference = earlier.get(index.checked_sub(delta_idx)?)?;
        let negative = b.flag()?;
        let abs_delta_rps = b.ue()?.checked_add(1).filter(|&d| d <= 1 << 15)?;
        let delta_rps = if negative {
            -(abs_delta_rps as i32)
        } else {
            abs_delta_rps as i32
        };

        // One entry per picture of the reference set, earlier ones first,
        // and a last one for the reference picture itself.
        let (s0, s1) = (&reference.before, &reference.after);
        let count = s0.len() + s1.len();
        let mut used = Vec::with_capacity(count + 1);
        let mut kept = Vec::with_capacity(count + 1);
        for _ in 0..=count {
            let used_by_curr = b.flag()?;
            used.push(used_by_curr);
            kept.push(used_by_curr || b.flag()?);
        }
        let own = |j: usize, poc: i32| (kept[j]).then_some((poc.checked_add(delta_rps)?, used[j]));

        for (j, &(poc, _)) in s1.iter().enumerate().rev() {
            if let Some(entry) = own(s0.len() + j, poc).filter(|e| e.0 < 0) {
                set.before.push(entry);
            }
        }
        if delta_rps < 0 && kept[count] {
            set.before.push((delta_rps, used[count]));
        }
        for (j, &(poc, _)) in s0.iter().enumerate() {
            if let Some(entry) = own(j, poc).filter(|e| e.0 < 0) {
                set.before.push(entry);
            }
        }

        for (j, &(poc, _)) in s0.iter().enumerate().rev() {
            if let Some(entry) = own(j, poc).filter(|e| e.0 > 0) {
                set.after.push(entry);
            }
        }
        if delta_rps > 0 && kept[count] {
            set.after.push((delta_rps, used[count]));
        }
        for (j, &(poc, _)) in s1.iter().enumerate() {
            if let Some(entry) = own(s0.len() + j, poc).filter(|e| e.0 > 0) {
                set.after.push(entry);
            }
        }
    } else {
        let negative = b.ue()? as usize;
        let positive = b.ue()? as usize;
        if negative.saturating_add(positive) > MAX_SET {
            return None;
        }
        let mut poc = 0i32;
        for _ in 0..negative {
            poc -= step(b)?;
            set.before.push((poc, b.flag()?));
        }
        poc = 0;
        for _ in 0..positive {
            poc += step(b)?;
            set.after.push((poc, b.flag()?));
        }
    }
    if set.before.len() + set.after.len() > MAX_SET {
        return None;
    }
    Some(set)
}

/// delta_poc_s0_minus1 or delta_poc_s1_minus1 plus 1, at most 2^15
/// (7.4.8).
fn step(b: &mut Bits<'_>) -> Option<i32> {
    Some(b.ue()?.checked_add(1).filter(|&d| d <= 1 << 15)? as i32)
}

/// What a PPS says that the slice headers depend on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pps {
    pub pps_id: u32,
    pub sps_id: u32,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u32,
    pub num_ref_idx_l0_default_active: u32,
    pub num_ref_idx_l1_default_active: u32,
    pub lists_modification_present: bool,
}

pub fn parse_pps(nal: &Nal<'_>) -> Option<Pps> {
    if kind(nal) != PPS {
        return None;
    }
    let mut b = payload(nal);
    let pps_id = b.ue()?;
    let sps_id = b.ue()?;
    if pps_id > 63 || sps_id > 15 {
        return None;
    }
    let dependent_slice_segments_enabled = b.flag()?;
    let output_flag_present = b.flag()?;
    let num_extra_slice_header_bits = b.bits(3)?;
    let _sign_data_hiding = b.flag()?;
    let _cabac_init_present = b.flag()?;
    // Up to 15 each (7.4.3.3.1).
    let num_ref_idx_l0_default_active = b.ue()?.checked_add(1).filter(|&n| n <= 15)?;
    let num_ref_idx_l1_default_active = b.ue()?.checked_add(1).filter(|&n| n <= 15)?;
    let _init_qp_minus26 = b.se()?;
    let _constrained_intra_pred = b.flag()?;
    let _transform_skip = b.flag()?;
    if b.flag()? {
        let _diff_cu_qp_delta_depth = b.ue()?;
    }
    let _cb_qp_offset = b.se()?;
    let _cr_qp_offset = b.se()?;
    let _slice_chroma_qp_offsets_present = b.flag()?;
    let _weighted_pred = b.flag()?;
    let _weighted_bipred = b.flag()?;
    let _transquant_bypass = b.flag()?;
    let tiles = b.flag()?;
    let _entropy_coding_sync = b.flag()?;
    if tiles {
        let columns = b.ue()?;
        let rows = b.ue()?;
        // Level 6.2 allows 20 columns and 22 rows (Table A.8).
        if columns >= 64 || rows >= 64 {
            return None;
        }
        if !b.flag()? {
            for _ in 0..columns + rows {
                let _width_or_height_minus1 = b.ue()?;
            }
        }
        let _loop_filter_across_tiles = b.flag()?;
    }
    let _loop_filter_across_slices = b.flag()?;
    if b.flag()? {
        let _deblocking_filter_override_enabled = b.flag()?;
        if !b.flag()? {
            let _beta_offset_div2 = b.se()?;
            let _tc_offset_div2 = b.se()?;
        }
    }
    if b.flag()? {
        skip_scaling_list_data(&mut b)?;
    }
    let lists_modification_present = b.flag()?;
    Some(Pps {
        pps_id,
        sps_id,
        dependent_slice_segments_enabled,
        output_flag_present,
        num_extra_slice_header_bits,
        num_ref_idx_l0_default_active,
        num_ref_idx_l1_default_active,
        lists_modification_present,
    })
}

/// The start of a slice segment header, as far as it says which pictures
/// the slice predicts from and which the decoder keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub first_in_picture: bool,
    pub slice_type: SliceType,
    pub pps_id: u32,
    /// slice_pic_order_cnt_lsb; 0 for an IDR, which carries none.
    pub pic_order_cnt_lsb: u32,
    /// The pictures kept from this one on. None for an IDR, which keeps
    /// none.
    pub ref_pic_set: Option<RefPicSet>,
    /// Long-term pictures the slice names, which Booth's encoders never
    /// use.
    pub long_term_pics: u32,
    /// How many entries of reference list 0 the slice may use, after any
    /// override in the header. Zero for I slices.
    pub num_ref_idx_l0_active: u32,
    /// list_entry_l0 when the slice reorders list 0: entry i of the list
    /// is entry list0_entries[i] of the list it would otherwise have.
    pub list0_entries: Option<Vec<u32>>,
}

/// Reads a slice segment header with the SPS and PPS it names. None when
/// the NAL unit is not a slice, names another PPS, ends early, or is a
/// dependent slice segment, which takes its header from the segment before
/// it.
pub fn slice_header(nal: &Nal<'_>, sps: &Sps, pps: &Pps) -> Option<SliceHeader> {
    if !is_slice(nal) {
        return None;
    }
    let mut b = payload(nal);
    let first_in_picture = b.flag()?;
    if is_irap(nal) {
        let _no_output_of_prior_pics = b.flag()?;
    }
    let pps_id = b.ue()?;
    if pps_id != pps.pps_id || pps.sps_id != sps.sps_id {
        return None;
    }
    if !first_in_picture {
        if pps.dependent_slice_segments_enabled && b.flag()? {
            return None;
        }
        let ctb = 1u64 << sps.log2_ctb_size;
        let ctbs =
            u64::from(sps.coded_width).div_ceil(ctb) * u64::from(sps.coded_height).div_ceil(ctb);
        let address_bits = bits_for(ctbs);
        if address_bits > 32 {
            return None;
        }
        let _slice_segment_address = b.bits(address_bits)?;
    }
    for _ in 0..pps.num_extra_slice_header_bits {
        let _slice_reserved = b.flag()?;
    }
    let slice_type = match b.ue()? {
        0 => SliceType::B,
        1 => SliceType::P,
        2 => SliceType::I,
        _ => return None,
    };
    if pps.output_flag_present {
        let _pic_output = b.flag()?;
    }
    if sps.separate_colour_plane {
        let _colour_plane_id = b.bits(2)?;
    }

    let mut pic_order_cnt_lsb = 0;
    let mut ref_pic_set_here = None;
    let mut long_term_pics = 0;
    let mut long_term_used = 0;
    if !is_idr(nal) {
        pic_order_cnt_lsb = b.bits(sps.log2_max_pic_order_cnt_lsb)?;
        let sets = &sps.short_term_ref_pic_sets;
        let set = if !b.flag()? {
            ref_pic_set(&mut b, sets.len(), sets, true)?
        } else {
            let index = b.bits(bits_for(sets.len() as u64))?;
            sets.get(index as usize)?.clone()
        };
        if sps.long_term_ref_pics_present {
            let listed = &sps.long_term_ref_pics_sps;
            let from_sps = if listed.is_empty() { 0 } else { b.ue()? };
            let own = b.ue()?;
            if from_sps as usize > listed.len() || own > 32 {
                return None;
            }
            for i in 0..from_sps + own {
                let used = if i < from_sps {
                    let at = b.bits(bits_for(listed.len() as u64))?;
                    *listed.get(at as usize)?
                } else {
                    let _poc_lsb_lt = b.bits(sps.log2_max_pic_order_cnt_lsb)?;
                    b.flag()?
                };
                long_term_used += u32::from(used);
                if b.flag()? {
                    let _delta_poc_msb_cycle_lt = b.ue()?;
                }
            }
            long_term_pics = from_sps + own;
        }
        if sps.temporal_mvp {
            let _slice_temporal_mvp = b.flag()?;
        }
        ref_pic_set_here = Some(set);
    }
    if sps.sample_adaptive_offset {
        let _sao_luma = b.flag()?;
        if sps.chroma_array_type() != 0 {
            let _sao_chroma = b.flag()?;
        }
    }

    let mut num_ref_idx_l0_active = 0;
    let mut list0_entries = None;
    if slice_type != SliceType::I {
        let bi = slice_type == SliceType::B;
        let mut l0 = pps.num_ref_idx_l0_default_active;
        let mut l1 = pps.num_ref_idx_l1_default_active;
        if b.flag()? {
            l0 = b.ue()?.checked_add(1)?;
            if bi {
                l1 = b.ue()?.checked_add(1)?;
            }
        }
        if l0 > 15 || l1 > 15 {
            return None;
        }
        let total =
            ref_pic_set_here.as_ref().map_or(0, RefPicSet::used) as u64 + u64::from(long_term_used);
        if pps.lists_modification_present && total > 1 {
            let entry_bits = bits_for(total);
            if b.flag()? {
                let entries = (0..l0)
                    .map(|_| b.bits(entry_bits))
                    .collect::<Option<Vec<_>>>()?;
                list0_entries = Some(entries);
            }
            if bi && b.flag()? {
                for _ in 0..l1 {
                    b.bits(entry_bits)?;
                }
            }
        }
        num_ref_idx_l0_active = l0;
    }

    Some(SliceHeader {
        first_in_picture,
        slice_type,
        pps_id,
        pic_order_cnt_lsb,
        ref_pic_set: ref_pic_set_here,
        long_term_pics,
        num_ref_idx_l0_active,
        list0_entries,
    })
}

#[cfg(test)]
mod tests {
    use super::super::Writer;
    use super::*;
    use proptest::strategy::Strategy;

    // forbidden_zero_bit, the type, layer 0, TemporalId 0.
    fn header(kind: u8) -> [u8; 2] {
        [kind << 1, 1]
    }

    // Zero bytes after a start code are padding, which nal_units skips, so
    // they are no picture, and an IDR after them is still the frame's first.
    #[test]
    fn padding_after_a_start_code_is_no_picture() {
        assert_eq!(first_picture(&[0, 3, 0, 0, 0, 1, 0, 0]), None);
        let mut unit = vec![0, 0, 1, 0, 0, 0, 1];
        unit.extend_from_slice(&header(IDR_W_RADL));
        unit.push(0xaf);
        assert_eq!(first_picture(&unit), Some(IDR_W_RADL));
        // A unit that starts with a zero byte and holds more is a picture.
        assert_eq!(first_picture(&[0, 0, 1, 0, 5]), Some(0));
    }

    /// An SPS as NVENC writes one for 2560x1440, Main, level 6, 12
    /// references, with a conformance window when `height` is not a
    /// multiple of 16.
    fn nvenc_like_sps(height: u32) -> Vec<u8> {
        let mut w = Writer::new();
        // VPS 0, one sub-layer, nesting.
        w.u(4, 0).u(3, 0).u(1, 1);
        // Main tier, Main profile, compatible with Main and Main 10,
        // progressive and frame-only, level 6.
        w.u(2, 0).u(1, 0).u(5, 1).u(32, 0x6000_0000);
        w.u(4, 0b1001).u(32, 0).u(12, 0).u(8, 180);
        // SPS 0, 4:2:0, 2560 wide, coded height in whole 16-sample blocks
        // as an encoder with larger coding blocks writes it.
        let coded = height.div_ceil(16) * 16;
        w.ue(0).ue(1).ue(2560).ue(coded);
        if coded == height {
            w.u(1, 0);
        } else {
            w.u(1, 1).ue(0).ue(0).ue(0).ue((coded - height) / 2);
        }
        // 8-bit, 8-bit POC lsb, 13 pictures held, nothing reordered.
        w.ue(0).ue(0).ue(4).u(1, 1).ue(12).ue(0).ue(0);
        // 8x8 minimum coding block, 32x32 tree blocks, transform sizes.
        w.ue(0).ue(2).ue(0).ue(3).ue(0).ue(0);
        // No scaling lists, AMP, SAO, no PCM, no sets, no long-term.
        w.u(1, 0).u(1, 1).u(1, 1).u(1, 0).ue(0).u(1, 0);
        // Temporal MVP, no strong smoothing, a VUI.
        w.u(1, 1).u(1, 0).u(1, 1);
        // No aspect ratio or overscan; unspecified format, limited range,
        // BT.709; then the rest of the VUI, which the reader leaves.
        w.u(1, 0).u(1, 0).u(1, 1).u(3, 5).u(1, 0).u(1, 1);
        w.u(8, 1).u(8, 1).u(8, 1);
        w.u(1, 0).u(1, 0).u(1, 0).u(1, 0).u(1, 0).u(1, 0).u(1, 0);
        w.nal_after(&header(SPS))
    }

    fn nvenc_like_pps() -> Vec<u8> {
        let mut w = Writer::new();
        // PPS 0 on SPS 0, no dependent segments, no output flag, no extra
        // bits, sign hiding, CABAC init present.
        w.ue(0).ue(0).u(1, 0).u(1, 0).u(3, 0).u(1, 1).u(1, 1);
        // One reference in each list by default.
        w.ue(0).ue(0).se(0).u(1, 0).u(1, 0);
        // cu_qp_delta with depth 1, no chroma offsets, no weighting.
        w.u(1, 1).ue(1).se(0).se(0).u(1, 0).u(1, 0).u(1, 0).u(1, 0);
        // No tiles or wavefronts, filter across slices, deblocking control
        // with no override, filter on, offsets 0.
        w.u(1, 0)
            .u(1, 0)
            .u(1, 1)
            .u(1, 1)
            .u(1, 0)
            .u(1, 0)
            .se(0)
            .se(0);
        // No scaling list, no list modification, merge level 2, no
        // extension.
        w.u(1, 0).u(1, 0).ue(0).u(1, 0).u(1, 0);
        w.nal_after(&header(PPS))
    }

    fn parse_sps_bytes(data: &[u8]) -> Sps {
        parse_sps(&Nal { data }).expect("a valid SPS")
    }

    #[test]
    fn reads_an_sps_like_nvencs() {
        let sps = parse_sps_bytes(&nvenc_like_sps(1440));
        assert_eq!(
            sps,
            Sps {
                sps_id: 0,
                vps_id: 0,
                profile_idc: 1,
                high_tier: false,
                level_idc: 180,
                chroma_format_idc: 1,
                separate_colour_plane: false,
                width: 2560,
                height: 1440,
                coded_width: 2560,
                coded_height: 1440,
                bit_depth_luma: 8,
                bit_depth_chroma: 8,
                log2_max_pic_order_cnt_lsb: 8,
                max_dec_pic_buffering: 13,
                max_num_reorder_pics: 0,
                max_latency_increase_plus1: 0,
                log2_min_cb_size: 3,
                log2_ctb_size: 5,
                sample_adaptive_offset: true,
                short_term_ref_pic_sets: Vec::new(),
                long_term_ref_pics_present: false,
                long_term_ref_pics_sps: Vec::new(),
                temporal_mvp: true,
                vui: Some(Vui {
                    full_range: Some(false),
                    colour: Some((1, 1, 1)),
                }),
            }
        );
    }

    #[test]
    fn conformance_window() {
        let sps = parse_sps_bytes(&nvenc_like_sps(1080));
        assert_eq!((sps.coded_width, sps.coded_height), (2560, 1088));
        assert_eq!((sps.width, sps.height), (2560, 1080));
    }

    #[test]
    fn coded_sizes_of_a_unit() {
        let start = [0u8, 0, 0, 1];
        let sps = nvenc_like_sps(1080);
        let slice = Writer::new().u(1, 1).ue(0).nal_after(&header(TRAIL_R));
        let unit = [&start[..], &sps, &start, &nvenc_like_pps(), &start, &slice].concat();
        let sizes: Vec<_> = coded_sizes(&unit).collect();
        assert_eq!(
            sizes,
            [Some((2560, 1088))],
            "the coded size, not the shown one"
        );

        // One after the slice counts too, and one cut short reads as None.
        let cut = &sps[..sps.len() / 2];
        let unit = [&unit[..], &start, cut].concat();
        let sizes: Vec<_> = coded_sizes(&unit).collect();
        assert_eq!(sizes, [Some((2560, 1088)), None]);

        let p_frame = [&start[..], &slice].concat();
        assert_eq!(coded_sizes(&p_frame).count(), 0);
    }

    fn spss_split_out(unit: &[u8]) -> usize {
        super::super::nal_units(unit)
            .filter(|nal| kind(nal) == SPS)
            .count()
    }

    #[test]
    fn sps_count_anywhere() {
        // Each place in three whole blocks of the count and the tail after
        // them, with and without the forbidden bit, and a PPS, which is no
        // SPS.
        for header in [0x42u8, 0x43, 0xc2, 0xc3, 0x44] {
            for at in 0..800 {
                let mut unit = vec![0xff; at];
                unit.extend_from_slice(&[0, 0, 1, header, 1]);
                unit.resize(805, 0xff);
                let found = sps_count(&unit);
                assert_eq!(found, spss_split_out(&unit), "{header:#x} at {at}");
                assert_eq!(found, usize::from(header != 0x44), "{header:#x} at {at}");
            }
        }
        // Its header byte the last of the unit.
        assert_eq!(sps_count(&[7, 0, 0, 1, 0x42]), 1);
        assert_eq!(sps_count(&[0, 0, 1]), 0);
        assert_eq!(sps_count(&[]), 0);
    }

    #[test]
    fn reads_nal_types() {
        let stream: Vec<u8> = [
            &[0, 0, 0, 1][..],
            &header(VPS),
            &[0xaa, 0, 0, 1],
            &header(SPS),
            &[0xbb, 0, 0, 1],
            &header(IDR_W_RADL),
            &[0xcc, 0, 0, 1],
            &header(TRAIL_R),
            &[0xdd],
        ]
        .concat();
        let nals: Vec<Nal<'_>> = super::super::nal_units(&stream).collect();
        let kinds: Vec<u8> = nals.iter().map(kind).collect();
        assert_eq!(kinds, [VPS, SPS, IDR_W_RADL, TRAIL_R]);
        let slices: Vec<bool> = nals.iter().map(is_slice).collect();
        assert_eq!(slices, [false, false, true, true]);
        assert!(is_idr(&nals[2]) && is_irap(&nals[2]) && !is_idr(&nals[3]));
        assert!(nals.iter().all(|n| temporal_id(n) == 0));
        assert!(!is_slice(&Nal { data: &[] }), "an empty unit is no slice");
        let cra = header(CRA);
        assert!(is_irap(&Nal { data: &cra }) && !is_idr(&Nal { data: &cra }));
    }

    #[test]
    fn reads_a_pps() {
        let data = nvenc_like_pps();
        assert_eq!(
            parse_pps(&Nal { data: &data }),
            Some(Pps {
                pps_id: 0,
                sps_id: 0,
                dependent_slice_segments_enabled: false,
                output_flag_present: false,
                num_extra_slice_header_bits: 0,
                num_ref_idx_l0_default_active: 1,
                num_ref_idx_l1_default_active: 1,
                lists_modification_present: false,
            })
        );
    }

    #[test]
    fn reads_slice_headers() {
        let sps = parse_sps_bytes(&nvenc_like_sps(1440));
        let pps_data = nvenc_like_pps();
        let pps = parse_pps(&Nal { data: &pps_data }).expect("PPS");

        // First segment, no_output_of_prior_pics, PPS 0, I, SAO on both.
        let idr = Writer::new()
            .u(1, 1)
            .u(1, 0)
            .ue(0)
            .ue(2)
            .u(1, 1)
            .u(1, 1)
            .nal_after(&header(IDR_W_RADL));
        assert_eq!(
            slice_header(&Nal { data: &idr }, &sps, &pps),
            Some(SliceHeader {
                first_in_picture: true,
                slice_type: SliceType::I,
                pps_id: 0,
                pic_order_cnt_lsb: 0,
                ref_pic_set: None,
                long_term_pics: 0,
                num_ref_idx_l0_active: 0,
                list0_entries: None,
            })
        );

        // A P frame at POC 5 keeping 4 and 3, predicting from 4 only, with
        // the list overridden to one entry.
        let p = Writer::new()
            .u(1, 1)
            .ue(0)
            .ue(1)
            .u(8, 5)
            .u(1, 0)
            .ue(2)
            .ue(0)
            .ue(0)
            .u(1, 1)
            .ue(0)
            .u(1, 0)
            .u(1, 1)
            .u(1, 1)
            .u(1, 1)
            .u(1, 1)
            .ue(0)
            .nal_after(&header(TRAIL_R));
        assert_eq!(
            slice_header(&Nal { data: &p }, &sps, &pps),
            Some(SliceHeader {
                first_in_picture: true,
                slice_type: SliceType::P,
                pps_id: 0,
                pic_order_cnt_lsb: 5,
                ref_pic_set: Some(RefPicSet {
                    before: vec![(-1, true), (-2, false)],
                    after: Vec::new(),
                }),
                long_term_pics: 0,
                num_ref_idx_l0_active: 1,
                list0_entries: None,
            })
        );

        // A header for another PPS is not read with this one.
        let other = Writer::new()
            .u(1, 1)
            .ue(3)
            .ue(1)
            .nal_after(&header(TRAIL_R));
        assert_eq!(slice_header(&Nal { data: &other }, &sps, &pps), None);
    }

    #[test]
    fn predicted_sets() {
        // The SPS's set 0 keeps the picture before; the slice's own set is
        // predicted from it one picture on (deltaRps -1): the old -1 is now
        // -2, and the reference picture itself comes in as -1.
        let mut sps = parse_sps_bytes(&nvenc_like_sps(1440));
        sps.short_term_ref_pic_sets = vec![RefPicSet {
            before: vec![(-1, true)],
            after: Vec::new(),
        }];
        sps.temporal_mvp = false;
        sps.sample_adaptive_offset = false;
        let pps_data = nvenc_like_pps();
        let pps = parse_pps(&Nal { data: &pps_data }).expect("PPS");
        let slice = Writer::new()
            .u(1, 1)
            .ue(0)
            .ue(1)
            .u(8, 9)
            .u(1, 0)
            // inter_ref_pic_set_prediction, delta_idx_minus1 0, negative,
            // abs_delta_rps_minus1 0.
            .u(1, 1)
            .ue(0)
            .u(1, 1)
            .ue(0)
            // Old -1: kept, not used now. The reference picture: used.
            .u(1, 0)
            .u(1, 1)
            .u(1, 1)
            .u(1, 0)
            .nal_after(&header(TRAIL_R));
        let header = slice_header(&Nal { data: &slice }, &sps, &pps).expect("a slice header");
        assert_eq!(
            header.ref_pic_set,
            Some(RefPicSet {
                before: vec![(-1, true), (-2, false)],
                after: Vec::new(),
            })
        );

        // The SPS's own sets predict from the one before them.
        let mut w = Writer::new();
        // Set 0: two before, both used.
        w.ue(2).ue(0).ue(0).u(1, 1).ue(0).u(1, 1);
        // Set 1: predicted from set 0 with deltaRps +1; every entry kept.
        w.u(1, 1).u(1, 0).ue(0).u(1, 1).u(1, 1).u(1, 1);
        let data = w.nal_after(&[0, 0]);
        let mut b = Bits::new(&data[2..]);
        let set0 = ref_pic_set(&mut b, 0, &[], false).expect("set 0");
        let set1 = ref_pic_set(&mut b, 1, std::slice::from_ref(&set0), false).expect("set 1");
        assert_eq!(set0.before, [(-1, true), (-2, true)]);
        // -1 and -2 move to 0 and -1; 0 is the current picture and is
        // dropped; the reference picture comes in after it at +1.
        assert_eq!(set1.before, [(-1, true)]);
        assert_eq!(set1.after, [(1, true)]);
    }

    #[test]
    fn sets_past_the_buffer() {
        let data = Writer::new().ue(17).ue(0).nal_after(&[0, 0]);
        let mut b = Bits::new(&data[2..]);
        assert_eq!(ref_pic_set(&mut b, 0, &[], false), None);
    }

    // Real parameter sets and slice headers with a few bytes changed and
    // the end cut anywhere reach much deeper into the readers than random
    // bytes do, which mostly stop at the first field out of range.
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(5000))]

        #[test]
        fn damaged_units_never_panic(
            changes in proptest::collection::vec((0usize..4096, 1u8..=255), 1..6),
            keep in 0usize..4096,
        ) {
            let sps = parse_sps_bytes(&nvenc_like_sps(1080));
            let pps_data = nvenc_like_pps();
            let pps = parse_pps(&Nal { data: &pps_data }).expect("PPS");
            let mut busy = sps.clone();
            busy.short_term_ref_pic_sets = vec![
                RefPicSet { before: vec![(-1, true), (-3, false)], after: Vec::new() },
                RefPicSet { before: vec![(-2, true)], after: vec![(2, true)] },
            ];
            busy.long_term_ref_pics_present = true;
            busy.long_term_ref_pics_sps = vec![true, false];
            let busy_pps = Pps {
                lists_modification_present: true,
                output_flag_present: true,
                dependent_slice_segments_enabled: true,
                num_extra_slice_header_bits: 2,
                ..pps.clone()
            };
            let slice = Writer::new()
                .u(1, 0)
                .ue(0)
                .u(1, 0)
                .u(12, 7)
                .ue(1)
                .u(8, 5)
                .u(1, 0)
                .u(1, 1)
                .ue(0)
                .u(1, 1)
                .ue(0)
                .u(1, 1)
                .u(1, 1)
                .u(1, 1)
                .ue(2)
                .nal_after(&header(TRAIL_R));
            for original in [nvenc_like_sps(1080), pps_data.clone(), slice] {
                let mut data = original.clone();
                for &(at, bits) in &changes {
                    let at = 2 + at % (data.len() - 2);
                    data[at] ^= bits;
                }
                data.truncate(2 + keep % (data.len() - 1));
                let nal = Nal { data: &data };
                let _ = parse_sps(&nal);
                let _ = parse_pps(&nal);
                let _ = slice_header(&nal, &sps, &pps);
                let _ = slice_header(&nal, &busy, &busy_pps);
            }
        }

        // Start codes of three and four bytes, SPS headers with and without
        // the forbidden bit, lone bytes that finish either, and noise, to
        // well past a block of the count.
        #[test]
        fn sps_count_matches_split(
            pieces in proptest::collection::vec(
                proptest::prop_oneof![
                    proptest::strategy::Just(vec![0u8, 0, 1, 0x42, 1]),
                    proptest::strategy::Just(vec![0u8, 0, 0, 1]),
                    proptest::strategy::Just(vec![0u8, 0, 1]),
                    proptest::strategy::Just(vec![0u8]),
                    proptest::strategy::Just(vec![0x42u8]),
                    proptest::strategy::Just(vec![0xc3u8]),
                    proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
                ],
                0..96,
            ),
        ) {
            let unit = pieces.concat();
            proptest::prop_assert_eq!(sps_count(&unit), spss_split_out(&unit));
        }

        // Whole and cut SPSs among start codes, zeros and noise, and runs
        // with no zero longer than a block, so an SPS can end blocks after
        // it starts.
        #[test]
        fn coded_sizes_match_split(
            pieces in proptest::collection::vec(
                proptest::prop_oneof![
                    proptest::strategy::Just([&[0u8, 0, 1][..], &nvenc_like_sps(1080)].concat()),
                    (3usize..30).prop_map(|keep| {
                        [&[0u8, 0, 1][..], &nvenc_like_sps(1440)[..keep]].concat()
                    }),
                    proptest::strategy::Just(vec![0u8, 0, 1, 0x42, 1]),
                    proptest::strategy::Just(vec![0u8, 0, 0, 1]),
                    proptest::strategy::Just(vec![0u8, 0, 1]),
                    proptest::strategy::Just(vec![0u8]),
                    proptest::strategy::Just(vec![0xffu8; 300]),
                    proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64),
                ],
                0..64,
            ),
        ) {
            let unit = pieces.concat();
            let split: Vec<_> = super::super::nal_units(&unit)
                .filter(|nal| kind(nal) == SPS)
                .map(|nal| parse_sps(&nal).map(|sps| (sps.coded_width, sps.coded_height)))
                .collect();
            proptest::prop_assert_eq!(coded_sizes(&unit).collect::<Vec<_>>(), split);
        }

        // Mostly SEIs, and runs with no start code longer than a block, so
        // the first picture can be blocks in or missing.
        #[test]
        fn first_picture_matches_walk(
            pieces in proptest::collection::vec(
                proptest::prop_oneof![
                    4 => proptest::strategy::Just(vec![0u8, 0, 1, PREFIX_SEI << 1, 1]),
                    1 => proptest::strategy::Just(vec![0u8, 0, 1]),
                    1 => proptest::strategy::Just(vec![0u8]),
                    1 => proptest::strategy::Just(vec![0xffu8; 300]),
                    2 => proptest::collection::vec(proptest::prelude::any::<u8>(), 0..8),
                ],
                0..160,
            ),
        ) {
            let unit = pieces.concat();
            // The first picture unit nal_units finds, as the decoder reads
            // the rest of the frame.
            let walked = super::super::nal_units(&unit)
                .find(is_slice)
                .map(|nal| kind(&nal));
            proptest::prop_assert_eq!(first_picture(&unit), walked);
        }
    }
}
