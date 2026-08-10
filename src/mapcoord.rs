//! World position -> the coordinate the game itself prints on the minimap.
//!
//! DROP_PC carries a raw `D3DXVECTOR3` in world units (e.g. -53.12, -47.67); the client's
//! MINIMAP_POSITION label shows a small grid pair instead ("21/16"). The two never match, so the
//! detector's position was unusable against what the player sees on screen.
//!
//! The conversion is the client's own, lifted from game.exe (the routine that feeds the
//! `"%s %d/%d"` minimap label):
//!
//! ```text
//!     grid_x = ((int)floor(pos.x) - origin_x) / 50
//!     grid_y = ((int)floor(pos.z) - origin_y) / 50
//! ```
//!
//! with C truncating division (Rust's `/` on i32 truncates the same way). `origin_x` / `origin_y`
//! are the SECOND number of `MAPSIZE_X` / `MAPSIZE_Y` in the map's `Data\map\<map>.mmp`, which the
//! client parses into the fields this routine reads. The table below is that pair for every map
//! whose id could be confirmed, joined from two of the client's own files:
//!   * `Data\glogic\mapslist.mst` — map id -> world (`.Lev`) file name, after undoing the
//!     EMBYTECRYPT byte substitution.
//!   * `Data\map\<name>.mmp`      — `MAPSIZE_X`/`MAPSIZE_Y` for that world file.
//!
//! Cross-check on a real DROP_PC capture: "-1st" on map 22 at world (-53.12, -47.67) gives
//! (-54 + 1122) / 50 = 21 and (-48 + 860) / 50 = 16 — the "21/16" the client would draw.

/// `(map id, origin_x, origin_y)` — the client's per-map axis origin. Maps missing here (channel
/// duplicates, PvP instances) simply keep showing raw world units; a wrong grid would be worse
/// than an honest one.
const MAP_ORIGIN: &[(u16, i32, i32)] = &[
    (0, -740, -980),      // SG_Campus1F        innerzone_01
    (1, -3091, -2578),    // SG_Campus          w_school_01
    (2, -3320, -4220),    // SacredGateHole     w_city_s_01
    (3, -740, -980),      // MP_Campus1F        innerzone_21
    (4, -3613, -3256),    // MP_Campus          w_school_02
    (5, -1560, -4100),    // MysticPeakHole     w_city_s_02
    (6, -740, -980),      // PhoenixCampus1F    innerzone_31
    (7, -5087, -5000),    // PhoenixCampus      w_school_03
    (8, -2771, -4810),    // PhoenixHole        w_city_s_03
    (9, -2650, -3266),    // LeonineCampus      w_school_04
    (10, -740, -980),     // LeonineCampus1F    w_school_04_in_1f
    (11, -710, -980),     // LeonineCampus2F    w_school_04_in_2f
    (12, -793, -983),     // LeonineCampus3F    w_school_04_in_3f
    (13, -510, -510),     // LeonineCampusB1    w_school_04_in_b1
    (14, -505, -505),     // LeonineCampusB2    w_school_04_in_b2
    (15, -4779, 127),     // TradingHole        w_city_C_01
    (16, -2851, -3324),   // SG HolePassage     w_city_s_tunnel
    (17, -3819, -3819),   // WharfPassage       w_city_D_01
    (18, -542, -320),     // Hangout 1F         w_blue_in_1F
    (19, -542, -540),     // Hangout 2F         w_blue_in_2F
    (20, -661, -660),     // Hangout 3F         w_blue_in_3F
    (21, -1542, -860),    // MarketPlace        w_tradezone1
    (22, -1122, -860),    // PracticingYard     w_Total_suryun
    (23, -1122, -860),    // Suryun             w_New_suryun
    (32, -5272, -3280),   // PrisonTestZone     prison_undercave
    (33, -638, -580),     // Labatory7          undercave_bossroom
    (34, -1802, -807),    // Shibuya            w_sibuya_new
    (35, -840, -939),     // Head.B 30F         w_ep3_saintB_30F
    (36, -840, -941),     // Head.B 50F         w_ep3_saintB_50F
    (37, -840, -941),     // Head.B 90F         w_ep3_saintB_90F
    (38, -1660, -662),    // Head.B Left Wall   w_ep3_saintB_left
    (39, -1660, -662),    // Head.B RightWall   w_ep3_saintB_right
    (40, -798, -1093),    // Director Room      w_ep3_saintB_boss1
    (41, -798, -1093),    // Director Room      w_ep3_saintB_boss2
    (42, -3729, -2385),   // Head.B U-ground    w_ep3_saintB_1B
    (43, -1683, -1045),   // Another W South    w_ep3_another_1
    (44, -1175, -850),    // Another W centre   w_ep3_another_2
    (45, -1683, -1045),   // Another W North    w_ep3_another_3
    (46, -1120, -1080),   // Head.B 1F          w_ep3_saintB_1F
    (47, -560, -560),     // Stadium            w_SchoolWar_01
    (48, -1683, -1045),   // Evil Zone          w_Special_01
    (51, -840, -941),     // Head.B 51F         w_ep3_saintB_51F
    (52, -840, -941),     // Head.B 52F         w_ep3_saintB_52F
    (95, -4640, -4640),   // Archer Cube        w_cube_lapse_4
    (100, -868, -803),    // StudyRoom1         w_school_01_in_08
    (101, -868, -803),    // StudyRoom2         w_school_01_in_08_2
    (102, -868, -803),    // Dormitory          w_school_01_trash
    (103, -868, -803),    // StudyRoom3         w_school_01_in_14
    (104, -868, -803),    // HistoryCentre      w_school_01_in_14_2
    (105, -868, -803),    // Library            w_school_01_in_17
    (106, -868, -803),    // SocietyRoom        w_school_01_in_22
    (107, -868, -803),    // ScienceCentre      w_school_01_in_31
    (120, -855, -787),    // StudyRoom2         w_school_02_in_08
    (121, -855, -787),    // StudyRoom1         w_school_02_in_08_2
    (122, -855, -787),    // Library            w_school_02_in_22
];

/// World units per minimap grid square. Hard-coded in the client as a divide-by-50.
const GRID: i32 = 50;

/// Axis origin for a map id, or None when that map isn't in the table.
fn origin(map: u16) -> Option<(i32, i32)> {
    MAP_ORIGIN.iter().find(|(id, _, _)| *id == map).map(|(_, x, y)| (*x, *y))
}

/// The minimap coordinate the client would print for a world position on `map`.
/// `world_x` / `world_z` are already floored (see `proximity::parse_pc`), matching the client's
/// `floor()` before the divide. None when the map's origin is unknown.
pub fn grid(map: u16, world_x: i32, world_z: i32) -> Option<(i32, i32)> {
    let (ox, oy) = origin(map)?;
    Some(((world_x - ox) / GRID, (world_z - oy) / GRID))
}

/// "21/16" like the client's minimap, or None when the map's origin is unknown.
pub fn grid_str(map: Option<u16>, world_x: Option<i32>, world_z: Option<i32>) -> Option<String> {
    let (gx, gy) = grid(map?, world_x?, world_z?)?;
    Some(format!("{}/{}", gx, gy))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference DROP_PC capture: "-1st" standing on map 22 at world (-53.12, -47.67), which
    /// `parse_pc` floors to (-54, -48). The client draws "21/16" for that spot.
    #[test]
    fn matches_the_client_minimap_label() {
        assert_eq!(grid(22, -54, -48), Some((21, 16)));
        // Same map, the far corner of the axis rect: origin itself is grid 0/0.
        assert_eq!(grid(22, -1122, -860), Some((0, 0)));
        // Truncation, not flooring, on the grid divide — this is C integer division, so a
        // position one unit outside the rect must land on 0, not -1.
        assert_eq!(grid(22, -1123, -861), Some((0, 0)));
        // A map with no entry keeps its raw world units rather than inventing a grid.
        assert_eq!(grid(999, 0, 0), None);
    }

    /// Every id appears once, so a lookup can't silently pick the wrong origin.
    #[test]
    fn map_ids_are_unique() {
        let mut ids: Vec<u16> = MAP_ORIGIN.iter().map(|(id, _, _)| *id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate map id in MAP_ORIGIN");
    }
}
