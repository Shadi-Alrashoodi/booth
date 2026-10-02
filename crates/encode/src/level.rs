// H.264 and HEVC levels: the limits a decoder is built for (Table A-1 of the
// H.264 standard, Tables A.8 and A.9 of HEVC's). The level is picked here
// rather than by the encoder because the reference frames Booth keeps for
// loss recovery count against it, and an encoder choosing on size and rate
// alone can declare a level whose decoded picture buffer is smaller than the
// references in use.

struct Level {
    idc: u32,
    max_mbps: u64,
    max_frame_mbs: u32,
    max_dpb_mbs: u32,
    // In units of 1000 bits a second, before the High profile factor.
    max_kbps: u32,
}

const fn level(
    idc: u32,
    max_mbps: u64,
    max_frame_mbs: u32,
    max_dpb_mbs: u32,
    max_kbps: u32,
) -> Level {
    Level {
        idc,
        max_mbps,
        max_frame_mbs,
        max_dpb_mbs,
        max_kbps,
    }
}

// From 4.0, which every GPU decoder of the last decade handles; the smaller
// levels would only matter below 720p. Columns: level_idc, macroblocks a
// second, macroblocks a frame, decoded picture buffer in macroblocks, bitrate.
const LEVELS: [Level; 9] = [
    level(40, 245_760, 8_192, 32_768, 20_000),
    level(41, 245_760, 8_192, 32_768, 50_000),
    level(42, 522_240, 8_704, 34_816, 50_000),
    level(50, 589_824, 22_080, 110_400, 135_000),
    level(51, 983_040, 36_864, 184_320, 240_000),
    level(52, 2_073_600, 36_864, 184_320, 240_000),
    level(60, 4_177_920, 139_264, 696_320, 240_000),
    level(61, 8_355_840, 139_264, 696_320, 480_000),
    level(62, 16_711_680, 139_264, 696_320, 800_000),
];

// The High profile allows 1.25 times the base bitrate (cpbBrVclFactor).
const HIGH_PROFILE_BITS_PER_KBPS: u64 = 1_250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Choice {
    /// level_idc: 52 is level 5.2.
    pub(crate) level: u32,
    pub(crate) references: u32,
}

/// Reference frames a level allows at a size (max_dec_frame_buffering).
fn dpb_frames(level: &Level, frame_mbs: u64) -> u32 {
    (u64::from(level.max_dpb_mbs) / frame_mbs).min(16) as u32
}

// In u64 because a caller can ask about any u32 size: macroblock counts
// multiplied in u32 overflow from about a million pixels a side.
fn macroblocks(width: u32, height: u32) -> (u64, u64, u64) {
    let width_mbs = u64::from(width.div_ceil(16));
    let height_mbs = u64::from(height.div_ceil(16));
    (width_mbs, height_mbs, width_mbs * height_mbs)
}

/// Whether a level holds the picture at the frame rate. The frame size
/// comes first, so the rate is only multiplied out for a picture that fits.
fn holds(level: &Level, width: u32, height: u32, fps: u32) -> bool {
    let (width_mbs, height_mbs, frame_mbs) = macroblocks(width, height);
    // A.3.1: neither side may be longer than sqrt(8 * MaxFS) macroblocks.
    let side_ok = |mbs: u64| mbs * mbs <= 8 * u64::from(level.max_frame_mbs);
    frame_mbs <= u64::from(level.max_frame_mbs)
        && side_ok(width_mbs)
        && side_ok(height_mbs)
        && frame_mbs * u64::from(fps) <= level.max_mbps
}

/// The lowest level that fits the picture, the frame rate, the bitrate and
/// `references` reference frames. If none does, the highest level with as
/// many references as it allows.
pub(crate) fn choose(width: u32, height: u32, fps: u32, bitrate: u32, references: u32) -> Choice {
    let frame_mbs = macroblocks(width, height).2.max(1);

    let fits = |level: &Level| {
        holds(level, width, height, fps)
            && u64::from(bitrate) <= u64::from(level.max_kbps) * HIGH_PROFILE_BITS_PER_KBPS
            && dpb_frames(level, frame_mbs) >= references
    };

    match LEVELS.iter().find(|l| fits(l)) {
        Some(level) => Choice {
            level: level.idc,
            references,
        },
        None => {
            let top = &LEVELS[LEVELS.len() - 1];
            Choice {
                level: top.idc,
                references: references.min(dpb_frames(top, frame_mbs)).max(1),
            }
        }
    }
}

/// Whether the top H.264 level holds this picture at this rate at all.
pub(crate) fn h264_fits(width: u32, height: u32, fps: u32) -> bool {
    holds(&LEVELS[LEVELS.len() - 1], width, height, fps)
}

struct HevcLevel {
    idc: u32,
    max_luma_ps: u64,
    max_luma_sr: u64,
    // In units of 1000 bits a second, the Main profile's CpbBrVclFactor.
    max_br_main: u32,
    max_br_high: u32,
}

const fn hevc_level(
    idc: u32,
    max_luma_ps: u64,
    max_luma_sr: u64,
    max_br_main: u32,
    max_br_high: u32,
) -> HevcLevel {
    HevcLevel {
        idc,
        max_luma_ps,
        max_luma_sr,
        max_br_main,
        max_br_high,
    }
}

// From 4, as for H.264. Columns: general_level_idc (30 times the level),
// luma samples a picture, luma samples a second, bitrate on the Main tier and
// on the High tier.
const HEVC_LEVELS: [HevcLevel; 8] = [
    hevc_level(120, 2_228_224, 66_846_720, 12_000, 30_000),
    hevc_level(123, 2_228_224, 133_693_440, 20_000, 50_000),
    hevc_level(150, 8_912_896, 267_386_880, 25_000, 100_000),
    hevc_level(153, 8_912_896, 534_773_760, 40_000, 160_000),
    hevc_level(156, 8_912_896, 1_069_547_520, 60_000, 240_000),
    hevc_level(180, 35_651_584, 1_069_547_520, 60_000, 240_000),
    hevc_level(183, 35_651_584, 2_139_095_040, 120_000, 480_000),
    hevc_level(186, 35_651_584, 4_278_190_080, 240_000, 800_000),
];

// maxDpbPicBuf for the Main profile (A.4.2).
const HEVC_DPB_PICTURES: u32 = 6;
// MaxDpbSize is 16 at most and holds the picture being decoded, so no HEVC
// level keeps more references than this, where H.264 keeps 16.
const HEVC_MAX_REFERENCES: u32 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HevcChoice {
    /// general_level_idc: 153 is level 5.1.
    pub(crate) level: u32,
    pub(crate) high_tier: bool,
    pub(crate) references: u32,
}

/// A side as HEVC codes it: whole minimum coding blocks, 8 by 8 at the
/// smallest, with the rest cropped.
fn hevc_side(side: u32) -> u64 {
    u64::from(side.div_ceil(8)) * 8
}

/// Luma samples of the coded picture. Saturating, since two sides near
/// u32::MAX multiply past u64; no level comes near either way.
fn hevc_samples(width: u32, height: u32) -> u64 {
    hevc_side(width).saturating_mul(hevc_side(height))
}

/// Reference frames a level allows at a size. HEVC's decoded picture buffer
/// (MaxDpbSize, A.4.2) holds the picture being decoded as well, so one less
/// than its size is left for references, where H.264's counts references
/// only.
fn hevc_references(level: &HevcLevel, samples: u64) -> u32 {
    let size = if samples <= level.max_luma_ps >> 2 {
        4 * HEVC_DPB_PICTURES
    } else if samples <= level.max_luma_ps >> 1 {
        2 * HEVC_DPB_PICTURES
    } else if samples <= (3 * level.max_luma_ps) >> 2 {
        4 * HEVC_DPB_PICTURES / 3
    } else {
        HEVC_DPB_PICTURES
    };
    (size - 1).min(HEVC_MAX_REFERENCES)
}

fn hevc_holds(level: &HevcLevel, width: u32, height: u32, fps: u32) -> bool {
    let samples = hevc_samples(width, height);
    // A.4.1: neither side may be longer than sqrt(8 * MaxLumaPs).
    let side_ok =
        |side: u32| hevc_side(side).saturating_mul(hevc_side(side)) <= 8 * level.max_luma_ps;
    samples <= level.max_luma_ps
        && side_ok(width)
        && side_ok(height)
        && samples * u64::from(fps) <= level.max_luma_sr
}

/// The lowest HEVC level and tier that fit the picture, the frame rate, the
/// bitrate and `references` reference frames, the Main tier first at each
/// level. More than HEVC_MAX_REFERENCES are asked for as that many, which
/// every level holds at a small enough picture. If none fits, the top level
/// on the High tier with as many references as it allows.
pub(crate) fn choose_hevc(
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    references: u32,
) -> HevcChoice {
    let references = references.min(HEVC_MAX_REFERENCES);
    let samples = hevc_samples(width, height);
    for level in &HEVC_LEVELS {
        if !hevc_holds(level, width, height, fps) || hevc_references(level, samples) < references {
            continue;
        }
        for (high_tier, max_br) in [(false, level.max_br_main), (true, level.max_br_high)] {
            if u64::from(bitrate) <= u64::from(max_br) * 1000 {
                return HevcChoice {
                    level: level.idc,
                    high_tier,
                    references,
                };
            }
        }
    }
    let top = &HEVC_LEVELS[HEVC_LEVELS.len() - 1];
    HevcChoice {
        level: top.idc,
        high_tier: true,
        references: references.min(hevc_references(top, samples)).max(1),
    }
}

/// Whether the top HEVC level holds this picture at this rate at all.
pub(crate) fn hevc_fits(width: u32, height: u32, fps: u32) -> bool {
    hevc_holds(&HEVC_LEVELS[HEVC_LEVELS.len() - 1], width, height, fps)
}

#[cfg(test)]
pub(crate) fn hevc_level_idcs() -> impl Iterator<Item = u32> {
    HEVC_LEVELS.iter().map(|l| l.idc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn booth_sizes() {
        // 2560x1440 is 14400 macroblocks; level 5.2 holds 184320 / 14400 =
        // 12 reference frames and 2073600 macroblocks a second (144 fps).
        assert_eq!(
            choose(2560, 1440, 120, 15_000_000, 12),
            Choice {
                level: 52,
                references: 12
            }
        );
        // 1080p60 fits 4.2 on rate, but 4.2 holds only 4 frames at 1080p.
        assert_eq!(
            choose(1920, 1080, 60, 8_000_000, 6),
            Choice {
                level: 50,
                references: 6
            }
        );
        assert_eq!(
            choose(1920, 1080, 120, 15_000_000, 12),
            Choice {
                level: 51,
                references: 12
            }
        );
        // Ultrawide 1440p at 120 is past 5.2's macroblock rate.
        assert_eq!(
            choose(3440, 1440, 120, 15_000_000, 12),
            Choice {
                level: 60,
                references: 12
            }
        );
    }

    #[test]
    fn more_references_move_up() {
        assert_eq!(choose(2560, 1440, 120, 15_000_000, 13).level, 60);
    }

    #[test]
    fn bitrate_counts() {
        assert_eq!(choose(1280, 720, 60, 20_000_000, 4).level, 40);
        assert_eq!(choose(1280, 720, 60, 30_000_000, 4).level, 41);
    }

    #[test]
    fn nothing_fits() {
        let c = choose(8192, 8192, 240, 15_000_000, 16);
        assert_eq!(c.level, 62);
        assert!(c.references >= 1 && c.references <= 16);
    }

    fn hevc(level: u32, high_tier: bool, references: u32) -> HevcChoice {
        HevcChoice {
            level,
            high_tier,
            references,
        }
    }

    #[test]
    fn hevc_booth_sizes() {
        // 1440p120 is 442 million luma samples a second, inside 5.1, but at
        // 1440p every level 5 buffer holds 12 pictures, the one being
        // decoded included, so 11 references. Twelve take level 6, whose
        // Main tier stops at 60 Mbit/s: the top of the upload setting needs
        // the High tier.
        assert_eq!(
            choose_hevc(2560, 1440, 120, 80_000_000, 12),
            hevc(180, true, 12)
        );
        assert_eq!(
            choose_hevc(2560, 1440, 120, 15_000_000, 11),
            hevc(153, false, 11)
        );
        // 1080p60 is past 4's sample rate and fills 4.1's buffer to 6
        // pictures, 5 references.
        assert_eq!(
            choose_hevc(1920, 1080, 60, 80_000_000, 6),
            hevc(150, true, 6)
        );
        assert_eq!(
            choose_hevc(1280, 720, 60, 8_000_000, 6),
            hevc(120, false, 6)
        );
    }

    #[test]
    fn hevc_counts_current_picture() {
        let samples = hevc_samples(2560, 1440);
        assert_eq!(hevc_references(&HEVC_LEVELS[3], samples), 11);
        assert_eq!(hevc_references(&HEVC_LEVELS[5], samples), 15);
        // A picture filling the level leaves maxDpbPicBuf less one.
        assert_eq!(hevc_references(&HEVC_LEVELS[3], 8_912_896), 5);
    }

    #[test]
    fn hevc_nothing_fits() {
        let c = choose_hevc(8192, 8192, 240, 15_000_000, 16);
        assert_eq!((c.level, c.high_tier), (186, true));
        assert!(c.references >= 1 && c.references <= 15);
        assert!(!hevc_fits(8192, 8192, 240));
        assert!(hevc_fits(7680, 2160, 120));
        assert!(h264_fits(7680, 2160, 120) && !h264_fits(8192, 8192, 240));
    }

    #[test]
    fn hevc_past_150_fps() {
        // NVENC asks for 16 references from 151 fps on, one more than any
        // HEVC level holds; 1080p240 fits 5.1 with 15.
        assert_eq!(
            choose_hevc(1920, 1080, 240, 80_000_000, 16),
            hevc(153, true, 15)
        );
        assert_eq!(
            choose_hevc(1280, 720, 240, 15_000_000, 16),
            hevc(150, false, 15)
        );
    }

    #[test]
    fn sizes_near_u32_max() {
        let huge = u32::MAX - 1;
        for (width, height) in [(huge, huge), (huge, 2), (2, huge), (1 << 20, 1 << 20)] {
            assert!(!h264_fits(width, height, 60), "{width}x{height}");
            assert!(!hevc_fits(width, height, 60), "{width}x{height}");
            assert_eq!(choose(width, height, 60, 15_000_000, 6).level, 62);
            assert_eq!(choose_hevc(width, height, 60, 15_000_000, 6).level, 186);
        }
    }
}
