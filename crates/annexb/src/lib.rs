//! A small reader for H.264 and HEVC in Annex B form: splits an access unit
//! into NAL units and reads the few header fields that say what a frame is
//! and what it predicts from. Any bytes at all give an answer, never a panic:
//! the encoders' checks and tests read their own output with it, and the
//! decoder reads every HEVC SPS a friend sends with it before FFmpeg sees
//! one. The H.264 side is here, HEVC's in [`hevc`]; the two share the
//! splitter and the bit reader.

#![forbid(unsafe_code)]

pub mod hevc;

/// The two codecs, for the few questions whose answer depends on which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
}

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;
pub const NAL_FILLER: u8 = 12;

/// One NAL unit: the header byte and the payload as they are on the wire,
/// emulation prevention bytes included, trailing zero bytes dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nal<'a> {
    pub data: &'a [u8],
}

impl<'a> Nal<'a> {
    /// nal_unit_type: 5 is an IDR slice, 1 any other slice, 7 an SPS.
    pub fn kind(&self) -> u8 {
        self.data.first().map_or(0, |b| b & 0x1f)
    }

    /// nal_ref_idc: zero for a picture nothing may predict from.
    pub fn ref_idc(&self) -> u8 {
        self.data.first().map_or(0, |b| (b >> 5) & 3)
    }

    pub fn is_slice(&self) -> bool {
        matches!(self.kind(), NAL_SLICE | NAL_IDR)
    }

    /// Whether this is picture data in `codec`: any slice of H.264, any VCL
    /// unit of HEVC.
    pub fn is_slice_in(&self, codec: Codec) -> bool {
        match codec {
            Codec::H264 => self.is_slice(),
            Codec::Hevc => hevc::is_slice(self),
        }
    }

    /// Whether this is a slice of an IDR picture in `codec`.
    pub fn is_idr_in(&self, codec: Codec) -> bool {
        match codec {
            Codec::H264 => self.kind() == NAL_IDR,
            Codec::Hevc => hevc::is_idr(self),
        }
    }

    fn payload(&self) -> Bits<'a> {
        Bits::new(self.data.get(1..).unwrap_or_default())
    }
}

pub fn nal_units(access_unit: &[u8]) -> NalUnits<'_> {
    NalUnits { rest: access_unit }
}

pub struct NalUnits<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = Nal<'a>;

    fn next(&mut self) -> Option<Nal<'a>> {
        loop {
            let start = self.rest.windows(3).position(|w| w == [0, 0, 1])? + 3;
            let after = &self.rest[start..];
            // A NAL unit ends where the next start code, or the zero byte
            // in front of a four-byte one, begins.
            let end = after
                .windows(3)
                .position(|w| w[0] == 0 && w[1] == 0 && w[2] <= 1)
                .unwrap_or(after.len());
            self.rest = &after[end..];
            let mut data = &after[..end];
            while let [rest @ .., 0] = data {
                data = rest;
            }
            if !data.is_empty() {
                return Some(Nal { data });
            }
        }
    }
}

// Where nal_units ends a NAL unit: at 00 00 00 or 00 00 01.
fn ends(w: &[u8], i: usize) -> u8 {
    u8::from((w[i] | w[i + 1] | (w[i + 2] & 0xfe)) == 0)
}

// How many positions find and hevc::sps_count test at once, in a form the
// compiler turns into SIMD compares.
const BLOCK: usize = 256;

// The first position where `hit` holds of the WIDTH bytes from there. A
// hostile unit can be 3 MiB with nothing in it a search is for, or one NAL
// unit 3 MiB long, so a block with no hit is passed over whole.
fn find<const WIDTH: usize>(data: &[u8], hit: impl Fn(&[u8], usize) -> u8) -> Option<usize> {
    let positions = (data.len() + 1).saturating_sub(WIDTH);
    let mut at = 0;
    while at + BLOCK <= positions {
        let window = &data[at..at + BLOCK + WIDTH - 1];
        let mut any = 0u8;
        for i in 0..BLOCK {
            any |= hit(window, i);
        }
        if any != 0 {
            return (0..BLOCK).position(|i| hit(window, i) != 0).map(|i| at + i);
        }
        at += BLOCK;
    }
    (at..positions).find(|&i| hit(data, i) != 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    fn from_code(code: u32) -> SliceType {
        match code % 5 {
            0 => SliceType::P,
            1 => SliceType::B,
            2 => SliceType::I,
            3 => SliceType::Sp,
            _ => SliceType::Si,
        }
    }

    fn predicts(self) -> bool {
        matches!(self, SliceType::P | SliceType::B | SliceType::Sp)
    }
}

/// The type of the first slice of a slice NAL unit.
pub fn slice_type(nal: &Nal<'_>) -> Option<SliceType> {
    if !nal.is_slice() {
        return None;
    }
    let mut bits = nal.payload();
    let _first_mb_in_slice = bits.ue()?;
    Some(SliceType::from_code(bits.ue()?))
}

/// What an SPS says about the stream, as far as Booth cares.
///
/// Every value is what the stream claims, unchecked: a damaged or hostile SPS
/// can claim a picture up to u32::MAX wide. Anything that sizes a texture or
/// a buffer from `width` and `height` has to bound them first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    /// 52 is level 5.2.
    pub level_idc: u8,
    pub sps_id: u32,
    pub width: u32,
    pub height: u32,
    pub max_num_ref_frames: u32,
    /// frame_num counts modulo 2 to the power of this.
    pub log2_max_frame_num: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb: u32,
    pub delta_pic_order_always_zero: bool,
    pub frame_mbs_only: bool,
    pub separate_colour_plane: bool,
    /// 0 when there is no chroma to weight: monochrome, or 4:4:4 coded as
    /// three separate planes.
    pub chroma_array_type: u32,
    pub vui: Option<Vui>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vui {
    pub full_range: Option<bool>,
    /// colour_primaries, transfer_characteristics, matrix_coefficients: 1,
    /// 1, 1 is BT.709.
    pub colour: Option<(u8, u8, u8)>,
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
}

// High profiles and the others that carry chroma format and scaling lists.
const PROFILES_WITH_CHROMA_INFO: [u32; 13] =
    [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

pub fn parse_sps(nal: &Nal<'_>) -> Option<Sps> {
    if nal.kind() != NAL_SPS {
        return None;
    }
    let mut b = nal.payload();

    let profile_idc = b.bits(8)?;
    let _constraint_flags = b.bits(8)?;
    let level_idc = b.bits(8)?;
    let sps_id = b.ue()?;

    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    if PROFILES_WITH_CHROMA_INFO.contains(&profile_idc) {
        chroma_format_idc = b.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = b.flag()?;
        }
        let _bit_depth_luma = b.ue()?;
        let _bit_depth_chroma = b.ue()?;
        let _transform_bypass = b.flag()?;
        if b.flag()? {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if b.flag()? {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    // Both lengths are 4 to 16 bits (7.4.2.1.1); anything longer is not an
    // SPS any decoder would take.
    let log2_max_frame_num = b.ue()?.checked_add(4).filter(|&n| n <= 16)?;
    let pic_order_cnt_type = b.ue()?;
    let mut log2_max_pic_order_cnt_lsb = 0;
    let mut delta_pic_order_always_zero = false;
    match pic_order_cnt_type {
        0 => {
            log2_max_pic_order_cnt_lsb = b.ue()?.checked_add(4).filter(|&n| n <= 16)?;
        }
        1 => {
            delta_pic_order_always_zero = b.flag()?;
            let _offset_for_non_ref_pic = b.se()?;
            let _offset_for_top_to_bottom_field = b.se()?;
            let cycle = b.ue()?;
            if cycle > 255 {
                return None;
            }
            for _ in 0..cycle {
                let _offset_for_ref_frame = b.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    let max_num_ref_frames = b.ue()?;
    let _gaps_allowed = b.flag()?;
    let width_mbs = u64::from(b.ue()?) + 1;
    let height_map_units = u64::from(b.ue()?) + 1;
    let frame_mbs_only = b.flag()?;
    if !frame_mbs_only {
        let _mb_adaptive_frame_field = b.flag()?;
    }
    let _direct_8x8_inference = b.flag()?;
    let (mut crop_x, mut crop_y) = (0u64, 0u64);
    if b.flag()? {
        let (left, right) = (u64::from(b.ue()?), u64::from(b.ue()?));
        let (top, bottom) = (u64::from(b.ue()?), u64::from(b.ue()?));
        // Crop offsets count in chroma samples (7.4.2.1.1).
        let field_factor = if frame_mbs_only { 1 } else { 2 };
        let (unit_x, unit_y) = match (chroma_format_idc, separate_colour_plane) {
            (0, _) | (3, true) => (1, field_factor),
            (1, _) => (2, 2 * field_factor),
            (2, _) => (2, field_factor),
            _ => (1, field_factor),
        };
        crop_x = unit_x * (left + right);
        crop_y = unit_y * (top + bottom);
    }
    let frame_height_mbs = height_map_units * if frame_mbs_only { 1 } else { 2 };
    let width = (width_mbs * 16).saturating_sub(crop_x);
    let height = (frame_height_mbs * 16).saturating_sub(crop_y);

    let vui = if b.flag()? {
        Some(parse_vui(&mut b)?)
    } else {
        None
    };

    Some(Sps {
        profile_idc: profile_idc as u8,
        level_idc: level_idc as u8,
        sps_id,
        width: u32::try_from(width).unwrap_or(u32::MAX),
        height: u32::try_from(height).unwrap_or(u32::MAX),
        max_num_ref_frames,
        log2_max_frame_num,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb,
        delta_pic_order_always_zero,
        frame_mbs_only,
        separate_colour_plane,
        chroma_array_type: if separate_colour_plane {
            0
        } else {
            chroma_format_idc
        },
        vui,
    })
}

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
    if b.flag()? {
        let _chroma_sample_loc_top = b.ue()?;
        let _chroma_sample_loc_bottom = b.ue()?;
    }
    if b.flag()? {
        let _num_units_in_tick = b.bits(32)?;
        let _time_scale = b.bits(32)?;
        let _fixed_frame_rate = b.flag()?;
    }
    let nal_hrd = b.flag()?;
    if nal_hrd {
        skip_hrd(b)?;
    }
    let vcl_hrd = b.flag()?;
    if vcl_hrd {
        skip_hrd(b)?;
    }
    if nal_hrd || vcl_hrd {
        let _low_delay_hrd = b.flag()?;
    }
    let _pic_struct_present = b.flag()?;
    if b.flag()? {
        let _mv_over_pic_boundaries = b.flag()?;
        let _max_bytes_per_pic_denom = b.ue()?;
        let _max_bits_per_mb_denom = b.ue()?;
        let _log2_max_mv_length_horizontal = b.ue()?;
        let _log2_max_mv_length_vertical = b.ue()?;
        vui.max_num_reorder_frames = Some(b.ue()?);
        vui.max_dec_frame_buffering = Some(b.ue()?);
    }
    Some(vui)
}

fn skip_hrd(b: &mut Bits<'_>) -> Option<()> {
    let cpb_count = b.ue()? + 1;
    if cpb_count > 32 {
        return None;
    }
    let _bit_rate_scale = b.bits(4)?;
    let _cpb_size_scale = b.bits(4)?;
    for _ in 0..cpb_count {
        let _bit_rate_value = b.ue()?;
        let _cpb_size_value = b.ue()?;
        let _cbr = b.flag()?;
    }
    // Four lengths of 5 bits each.
    b.bits(20)?;
    Some(())
}

/// Bits needed to write a number below `n`, Ceil(Log2(n)) in the standards.
fn bits_for(n: u64) -> u32 {
    if n <= 1 {
        0
    } else {
        64 - (n - 1).leading_zeros()
    }
}

fn skip_scaling_list(b: &mut Bits<'_>, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            let delta = i64::from(b.se()?);
            next = (last + delta).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// What a PPS says that the slice headers depend on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pps {
    pub pps_id: u32,
    pub sps_id: u32,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_l0_default_active: u32,
    pub num_ref_idx_l1_default_active: u32,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub redundant_pic_cnt_present: bool,
}

pub fn parse_pps(nal: &Nal<'_>) -> Option<Pps> {
    if nal.kind() != NAL_PPS {
        return None;
    }
    let mut b = nal.payload();
    let pps_id = b.ue()?;
    let sps_id = b.ue()?;
    let _entropy_coding_mode = b.flag()?;
    let bottom_field_pic_order_in_frame_present = b.flag()?;
    // Slice groups are a Baseline profile tool no GPU encoder writes, and
    // they change what follows.
    if b.ue()? != 0 {
        return None;
    }
    let num_ref_idx_l0_default_active = b.ue()?.checked_add(1)?;
    let num_ref_idx_l1_default_active = b.ue()?.checked_add(1)?;
    let weighted_pred = b.flag()?;
    let weighted_bipred_idc = b.bits(2)?;
    let _pic_init_qp = b.se()?;
    let _pic_init_qs = b.se()?;
    let _chroma_qp_index_offset = b.se()?;
    let _deblocking_filter_control_present = b.flag()?;
    let _constrained_intra_pred = b.flag()?;
    let redundant_pic_cnt_present = b.flag()?;
    Some(Pps {
        pps_id,
        sps_id,
        bottom_field_pic_order_in_frame_present,
        num_ref_idx_l0_default_active,
        num_ref_idx_l1_default_active,
        weighted_pred,
        weighted_bipred_idc,
        redundant_pic_cnt_present,
    })
}

/// The start of a slice header, as far as it says which pictures the slice
/// predicts from and which the decoder keeps afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub slice_type: SliceType,
    pub pps_id: u32,
    pub frame_num: u32,
    /// How many entries of reference list 0 the slice may use, after any
    /// override in the header. Zero for I and SI slices.
    pub num_ref_idx_l0_active: u32,
    /// ref_pic_list_modification of list 0, in order. Empty when the slice
    /// keeps the default order, newest picture first.
    pub list0_changes: Vec<ListChange>,
    /// The memory management operations when the slice itself says which
    /// references go. None when the oldest goes once the buffer is full (the
    /// sliding window), and for IDRs and pictures that are not references.
    pub memory_ops: Option<Vec<MemoryOp>>,
}

/// One step of ref_pic_list_modification: the next entry of the list is the
/// short-term picture this many picture numbers below or above the one the
/// step before chose (the current picture, for the first step), or a
/// long-term picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListChange {
    Down(u32),
    Up(u32),
    LongTerm(u32),
}

/// One memory_management_control_operation. Only the one Booth checks
/// carries its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOp {
    /// Operation 1: the short-term picture this many picture numbers below
    /// the current one is no longer a reference.
    ForgetShortTerm(u32),
    Other(u32),
}

/// Reads a slice header with the SPS and PPS it names. None when the NAL
/// unit is not a slice, names another PPS, or ends early.
pub fn slice_header(nal: &Nal<'_>, sps: &Sps, pps: &Pps) -> Option<SliceHeader> {
    if !nal.is_slice() {
        return None;
    }
    let idr = nal.kind() == NAL_IDR;
    let mut b = nal.payload();
    let _first_mb_in_slice = b.ue()?;
    let slice_type = SliceType::from_code(b.ue()?);
    let pps_id = b.ue()?;
    if pps_id != pps.pps_id || pps.sps_id != sps.sps_id {
        return None;
    }
    if sps.separate_colour_plane {
        let _colour_plane_id = b.bits(2)?;
    }
    let frame_num = b.bits(sps.log2_max_frame_num)?;
    let mut field_pic = false;
    if !sps.frame_mbs_only {
        field_pic = b.flag()?;
        if field_pic {
            let _bottom_field = b.flag()?;
        }
    }
    if idr {
        let _idr_pic_id = b.ue()?;
    }
    let bottom_delta = pps.bottom_field_pic_order_in_frame_present && !field_pic;
    if sps.pic_order_cnt_type == 0 {
        let _pic_order_cnt_lsb = b.bits(sps.log2_max_pic_order_cnt_lsb)?;
        if bottom_delta {
            let _delta_pic_order_cnt_bottom = b.se()?;
        }
    }
    if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero {
        let _delta_pic_order_cnt_0 = b.se()?;
        if bottom_delta {
            let _delta_pic_order_cnt_1 = b.se()?;
        }
    }
    if pps.redundant_pic_cnt_present {
        let _redundant_pic_cnt = b.ue()?;
    }

    let bi = slice_type == SliceType::B;
    if bi {
        let _direct_spatial_mv_pred = b.flag()?;
    }
    let mut l0 = pps.num_ref_idx_l0_default_active;
    let mut l1 = pps.num_ref_idx_l1_default_active;
    if slice_type.predicts() && b.flag()? {
        l0 = b.ue()?.checked_add(1)?;
        if bi {
            l1 = b.ue()?.checked_add(1)?;
        }
    }
    // A frame's lists hold 32 entries at most (7.4.3).
    if l0 > 32 || l1 > 32 {
        return None;
    }

    let mut list0_changes = Vec::new();
    if slice_type.predicts() && b.flag()? {
        list0_changes = list_changes(&mut b)?;
    }
    if bi && b.flag()? {
        list_changes(&mut b)?;
    }
    let p = matches!(slice_type, SliceType::P | SliceType::Sp);
    if (pps.weighted_pred && p) || (pps.weighted_bipred_idc == 1 && bi) {
        let lists = [l0, if bi { l1 } else { 0 }];
        skip_pred_weight_table(&mut b, sps.chroma_array_type != 0, lists)?;
    }

    let mut memory_ops = None;
    if nal.ref_idc() != 0 {
        if idr {
            let _no_output_of_prior_pics = b.flag()?;
            let _long_term_reference = b.flag()?;
        } else if b.flag()? {
            memory_ops = Some(memory_ops_of(&mut b)?);
        }
    }

    Some(SliceHeader {
        slice_type,
        pps_id,
        frame_num,
        num_ref_idx_l0_active: if slice_type.predicts() { l0 } else { 0 },
        list0_changes,
        memory_ops,
    })
}

// Every step costs at least a bit, so the loops below end with the data
// anyway; the caps keep a hostile header from growing a list past what a real
// one can hold.
const MAX_LIST_CHANGES: usize = 33;
const MAX_MEMORY_OPS: usize = 64;

fn list_changes(b: &mut Bits<'_>) -> Option<Vec<ListChange>> {
    let mut changes = Vec::new();
    loop {
        let change = match b.ue()? {
            0 => ListChange::Down(b.ue()?.checked_add(1)?),
            1 => ListChange::Up(b.ue()?.checked_add(1)?),
            2 => ListChange::LongTerm(b.ue()?),
            3 => return Some(changes),
            _ => return None,
        };
        if changes.len() == MAX_LIST_CHANGES {
            return None;
        }
        changes.push(change);
    }
}

fn memory_ops_of(b: &mut Bits<'_>) -> Option<Vec<MemoryOp>> {
    let mut ops = Vec::new();
    loop {
        let op = match b.ue()? {
            0 => return Some(ops),
            1 => MemoryOp::ForgetShortTerm(b.ue()?.checked_add(1)?),
            n @ (2 | 4 | 6) => {
                b.ue()?;
                MemoryOp::Other(n)
            }
            3 => {
                b.ue()?;
                b.ue()?;
                MemoryOp::Other(3)
            }
            5 => MemoryOp::Other(5),
            _ => return None,
        };
        if ops.len() == MAX_MEMORY_OPS {
            return None;
        }
        ops.push(op);
    }
}

fn skip_pred_weight_table(b: &mut Bits<'_>, chroma: bool, lists: [u32; 2]) -> Option<()> {
    let _luma_log2_weight_denom = b.ue()?;
    if chroma {
        let _chroma_log2_weight_denom = b.ue()?;
    }
    for entries in lists {
        for _ in 0..entries {
            if b.flag()? {
                b.se()?;
                b.se()?;
            }
            if chroma && b.flag()? {
                for _ in 0..4 {
                    b.se()?;
                }
            }
        }
    }
    Some(())
}

/// Reads a NAL unit's payload bit by bit as the encoder meant it, dropping
/// emulation prevention on the way (00 00 03 on the wire is 00 00 in the
/// payload), so nothing is copied and only the bytes a field needs are read.
struct Bits<'a> {
    data: &'a [u8],
    next: usize,
    // Zero bytes in a row just before `next`, counted up to two.
    zeros: u8,
    byte: u8,
    left: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Bits<'a> {
        Bits {
            data,
            next: 0,
            zeros: 0,
            byte: 0,
            left: 0,
        }
    }

    fn bit(&mut self) -> Option<u32> {
        if self.left == 0 {
            let mut byte = *self.data.get(self.next)?;
            self.next += 1;
            if self.zeros == 2 && byte == 3 {
                byte = *self.data.get(self.next)?;
                self.next += 1;
                self.zeros = 0;
            }
            self.zeros = if byte == 0 {
                (self.zeros + 1).min(2)
            } else {
                0
            };
            self.byte = byte;
            self.left = 8;
        }
        self.left -= 1;
        Some(u32::from(self.byte >> self.left) & 1)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.bit()? == 1)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        let mut value = 0u64;
        for _ in 0..n {
            value = (value << 1) | u64::from(self.bit()?);
        }
        Some(value as u32)
    }

    /// Unsigned Exp-Golomb, ue(v).
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        Some(((1u64 << zeros) - 1 + u64::from(rest)) as u32)
    }

    /// Signed Exp-Golomb, se(v).
    fn se(&mut self) -> Option<i32> {
        let k = i64::from(self.ue()?);
        let value = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        Some(value as i32)
    }
}

/// Writes the bits an encoder would, to build test NAL units.
#[cfg(test)]
pub(crate) struct Writer {
    bits: Vec<bool>,
}

#[cfg(test)]
impl Writer {
    pub(crate) fn new() -> Writer {
        Writer { bits: Vec::new() }
    }

    pub(crate) fn u(&mut self, n: u32, value: u32) -> &mut Self {
        for i in (0..n).rev() {
            self.bits.push((value >> i) & 1 == 1);
        }
        self
    }

    pub(crate) fn ue(&mut self, value: u32) -> &mut Self {
        let v = u64::from(value) + 1;
        let len = 64 - v.leading_zeros();
        for _ in 1..len {
            self.bits.push(false);
        }
        for i in (0..len).rev() {
            self.bits.push((v >> i) & 1 == 1);
        }
        self
    }

    pub(crate) fn se(&mut self, value: i32) -> &mut Self {
        let k = if value > 0 { 2 * value - 1 } else { -2 * value };
        self.ue(k as u32)
    }

    /// rbsp_trailing_bits, emulation prevention, header byte.
    pub(crate) fn nal(&mut self, header: u8) -> Vec<u8> {
        self.nal_after(&[header])
    }

    /// As nal(), for HEVC's two header bytes.
    pub(crate) fn nal_after(&mut self, header: &[u8]) -> Vec<u8> {
        self.bits.push(true);
        while !self.bits.len().is_multiple_of(8) {
            self.bits.push(false);
        }
        let rbsp: Vec<u8> = self
            .bits
            .chunks(8)
            .map(|c| c.iter().fold(0, |acc, &b| (acc << 1) | u8::from(b)))
            .collect();
        let mut out = header.to_vec();
        let mut zeros = 0;
        for byte in rbsp {
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            out.push(byte);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer() -> Writer {
        Writer::new()
    }

    fn nvenc_like_sps() -> Vec<u8> {
        let mut w = writer();
        w.u(8, 100).u(8, 0).u(8, 52).ue(0);
        // chroma 4:2:0, 8 bit, no bypass, one scaling list sent.
        w.ue(1).ue(0).ue(0).u(1, 0).u(1, 1);
        w.u(1, 1);
        for _ in 0..16 {
            w.se(0);
        }
        for _ in 1..8 {
            w.u(1, 0);
        }
        // 8-bit frame_num, POC type 0 with 6 bits, 12 references,
        // 2560x1088 cropped to 1080.
        w.ue(4).ue(0).ue(2).ue(12).u(1, 0);
        w.ue(159).ue(67).u(1, 1).u(1, 1);
        w.u(1, 1).ue(0).ue(0).ue(0).ue(4);
        // VUI: square pixels, limited range BT.709, timing, one NAL HRD.
        w.u(1, 1).u(1, 1).u(8, 1);
        w.u(1, 0);
        w.u(1, 1).u(3, 5).u(1, 0).u(1, 1).u(8, 1).u(8, 1).u(8, 1);
        w.u(1, 0);
        w.u(1, 1).u(32, 1).u(32, 240).u(1, 1);
        w.u(1, 1)
            .ue(0)
            .u(4, 0)
            .u(4, 0)
            .ue(117_187)
            .ue(117_187)
            .u(1, 1)
            .u(20, 0x5_2945);
        w.u(1, 0).u(1, 0).u(1, 0);
        w.u(1, 1).u(1, 1).ue(0).ue(0).ue(16).ue(16).ue(0).ue(12);
        w.nal(0x67)
    }

    fn nvenc_like_pps() -> Vec<u8> {
        let mut w = writer();
        // pps 0, sps 0, CABAC, no bottom delta, no slice groups.
        w.ue(0).ue(0).u(1, 1).u(1, 0).ue(0);
        // 12 and 1 references by default, no weighting.
        w.ue(11).ue(0).u(1, 0).u(2, 0);
        w.se(0).se(0).se(0).u(1, 1).u(1, 0).u(1, 0);
        // transform_8x8_mode and the rest, which the reader stops before.
        w.u(1, 1).u(1, 0).se(0);
        w.nal(0x68)
    }

    #[test]
    fn reads_an_sps_like_nvencs() {
        let nal = nvenc_like_sps();
        let mut stream = vec![0, 0, 0, 1];
        stream.extend_from_slice(&nal);
        stream.extend_from_slice(&[0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
        let nals: Vec<Nal<'_>> = nal_units(&stream).collect();
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].data, &nal[..]);
        assert_eq!(nals[1].kind(), NAL_PPS);

        let sps = parse_sps(&nals[0]).expect("a valid SPS");
        assert_eq!(
            sps,
            Sps {
                profile_idc: 100,
                level_idc: 52,
                sps_id: 0,
                width: 2560,
                height: 1080,
                max_num_ref_frames: 12,
                log2_max_frame_num: 8,
                pic_order_cnt_type: 0,
                log2_max_pic_order_cnt_lsb: 6,
                delta_pic_order_always_zero: false,
                frame_mbs_only: true,
                separate_colour_plane: false,
                chroma_array_type: 1,
                vui: Some(Vui {
                    full_range: Some(false),
                    colour: Some((1, 1, 1)),
                    max_num_reorder_frames: Some(0),
                    max_dec_frame_buffering: Some(12),
                }),
            }
        );
    }

    #[test]
    fn reads_a_pps() {
        let nal = nvenc_like_pps();
        let pps = parse_pps(&Nal { data: &nal }).expect("a valid PPS");
        assert_eq!(
            pps,
            Pps {
                pps_id: 0,
                sps_id: 0,
                bottom_field_pic_order_in_frame_present: false,
                num_ref_idx_l0_default_active: 12,
                num_ref_idx_l1_default_active: 1,
                weighted_pred: false,
                weighted_bipred_idc: 0,
                redundant_pic_cnt_present: false,
            }
        );
    }

    #[test]
    fn reads_slice_types() {
        let idr = writer().ue(0).ue(7).nal(0x65);
        let p = writer().ue(0).ue(5).nal(0x41);
        let stream: Vec<u8> = [&[0, 0, 0, 1][..], &idr, &[0, 0, 1], &p].concat();
        let types: Vec<_> = nal_units(&stream)
            .map(|n| (n.kind(), slice_type(&n)))
            .collect();
        assert_eq!(
            types,
            [
                (NAL_IDR, Some(SliceType::I)),
                (NAL_SLICE, Some(SliceType::P))
            ]
        );
    }

    #[test]
    fn reads_slice_headers() {
        let sps_nal = nvenc_like_sps();
        let pps_nal = nvenc_like_pps();
        let sps = parse_sps(&Nal { data: &sps_nal }).expect("SPS");
        let pps = parse_pps(&Nal { data: &pps_nal }).expect("PPS");

        // IDR: frame_num 0, idr_pic_id, POC, then the IDR's own marking.
        let idr = writer()
            .ue(0)
            .ue(7)
            .ue(0)
            .u(8, 0)
            .ue(0)
            .u(6, 0)
            .u(1, 0)
            .u(1, 0)
            .nal(0x65);
        assert_eq!(
            slice_header(&Nal { data: &idr }, &sps, &pps),
            Some(SliceHeader {
                slice_type: SliceType::I,
                pps_id: 0,
                frame_num: 0,
                num_ref_idx_l0_active: 0,
                list0_changes: Vec::new(),
                memory_ops: None,
            })
        );

        // A P slice that keeps the default list and the sliding window.
        let plain = writer()
            .ue(0)
            .ue(5)
            .ue(0)
            .u(8, 59)
            .u(6, 54)
            .u(1, 0)
            .u(1, 0)
            .u(1, 0)
            .nal(0x41);
        assert_eq!(
            slice_header(&Nal { data: &plain }, &sps, &pps),
            Some(SliceHeader {
                slice_type: SliceType::P,
                pps_id: 0,
                frame_num: 59,
                num_ref_idx_l0_active: 12,
                list0_changes: Vec::new(),
                memory_ops: None,
            })
        );

        // One reference, moved to four below the current picture, and three
        // references forgotten.
        let recovering = writer()
            .ue(0)
            .ue(5)
            .ue(0)
            .u(8, 60)
            .u(6, 56)
            .u(1, 1)
            .ue(0)
            .u(1, 1)
            .ue(0)
            .ue(3)
            .ue(3)
            .u(1, 1)
            .ue(1)
            .ue(0)
            .ue(1)
            .ue(1)
            .ue(1)
            .ue(2)
            .ue(0)
            .nal(0x41);
        assert_eq!(
            slice_header(&Nal { data: &recovering }, &sps, &pps),
            Some(SliceHeader {
                slice_type: SliceType::P,
                pps_id: 0,
                frame_num: 60,
                num_ref_idx_l0_active: 1,
                list0_changes: vec![ListChange::Down(4)],
                memory_ops: Some(vec![
                    MemoryOp::ForgetShortTerm(1),
                    MemoryOp::ForgetShortTerm(2),
                    MemoryOp::ForgetShortTerm(3),
                ]),
            })
        );

        // A header for another PPS is not read with this one.
        let other = writer().ue(0).ue(5).ue(1).u(8, 3).nal(0x41);
        assert_eq!(slice_header(&Nal { data: &other }, &sps, &pps), None);
    }

    #[test]
    fn splits_on_start_codes() {
        let stream = [
            9, 9, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 0, 1, 0x68, 5, 0, 0,
        ];
        let nals: Vec<&[u8]> = nal_units(&stream).map(|n| n.data).collect();
        assert_eq!(nals, [&[0x09, 0xf0][..], &[0x67, 1, 2], &[0x68, 5]]);
    }

    #[test]
    fn emulation_prevention_is_undone() {
        let mut bits = Bits::new(&[0, 0, 3, 1, 0, 0, 3, 0, 3]);
        let bytes: Vec<u32> = std::iter::from_fn(|| bits.bits(8)).collect();
        assert_eq!(bytes, [0, 0, 1, 0, 0, 0, 3]);

        // An escape at the very end leaves nothing to read.
        let mut bits = Bits::new(&[0, 0, 3]);
        assert_eq!(bits.bits(16), Some(0));
        assert_eq!(bits.bit(), None);
    }

    #[test]
    fn slice_type_stops_early() {
        // A big slice whose bytes past the first two are never looked at:
        // the two values sit in the first byte.
        let mut slice = writer().ue(0).ue(5).nal(0x41);
        slice.resize(200_000, 0xff);
        let nal = Nal { data: &slice };
        let mut bits = nal.payload();
        assert_eq!(bits.ue(), Some(0));
        assert_eq!(bits.ue(), Some(5));
        assert_eq!(bits.next, 1);
        assert_eq!(slice_type(&nal), Some(SliceType::P));
    }
}
