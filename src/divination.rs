//! 占卜工具的核心算法与数据，忠实移植自 ESP-Handheld 固件
//! （`main/modules/iching/`，小智手持机版周易六十四卦 + 小六壬）。
//!
//! 与固件的差异（交互从"摇一摇/按键 + 屏幕"变为语音客户端）：
//! - 摇一摇产生的硬件随机数换为 `rand::random`；
//! - 屏幕上的六个分类按钮换为 `iching_cast` 的 `category` 参数；
//! - 小六壬的公历转农历不再用固件里只覆盖 2024–2034 且漏 2025 闰六月的
//!   查表，改用 `chinese-lunisolar-calendar`（1901–2101 全历表）。

use chinese_lunisolar_calendar::{LunisolarDate, SolarDate};
use chrono::{Datelike, Local, Timelike};
use rand::random;
use serde_json::{json, Value};

mod iching_data;
use iching_data::{binary_to_index, ICHING};

pub const CATEGORIES: [&str; 6] = ["运势", "事业", "经商", "求名", "婚恋", "决策"];

/// 六神名称，下标即落位 0–5
const LIUREN_POS: [(&str, &str); 6] = [
    ("大安", "大吉"),
    ("留连", "拖延"),
    ("速喜", "喜事"),
    ("赤口", "争执"),
    ("小吉", "好结果"),
    ("空亡", "大不利"), // 固件里误作"大利"，与断曰矛盾，此处修正
];

/// 六神完整断曰，顺序与 [`LIUREN_POS`] 一致
const LIUREN_JUDGMENTS: [&str; 6] = [
    "身未动时，属木青龙，谋事一五七，贵人西南，冲犯东方，小孩婆姐六畜惊，大人青面阴神。断曰：大安事事昌，求财在坤方，失物去不远，宅舍保安康，行人身未动，病者主无妨，将军回田野，仔细更推详。",
    "卒未归时，属水玄武，谋事二八十，贵人南方，冲犯北方，小孩游路亡魂，大人乌面夫人。断曰：留连事难成，求谋日未明，官事宜速决，失者去不远，行者早回心，三分靠运气，七分靠打拼，凡事莫强求，安稳才是真。",
    "人便至时，属火朱雀，谋事三六九，贵人西南，冲犯南方，小孩婆姐动勿惊，大人火箭将军。断曰：速喜喜来临，求财向南行，失物申未午，逢人路上寻，官事有福德，病者无祸侵，田宅六畜吉，行人有信音。",
    "官事凶时，属金白虎，谋事四七十，贵人东方，冲犯西方，小孩迷魂童子，大人金神七煞。断曰：赤口主口舌，官非切要防，失物急去寻，行人有惊慌，鸡犬多作怪，病者出西方，更须防咀咒，恐怕染瘟殃。",
    "人来喜时，属木六合，谋事一五七，贵人西南，冲犯东方，小孩婆姐六畜惊，大人无主家神。断曰：小吉最吉昌，路上好商量，阳人来报喜，失物在坤方，行人立便至，交关真是强，凡事皆和合，病者守无妨。",
    "音信稀时，属土勾陈，谋事三六九，贵人北方，冲犯厝地，小孩土瘟神煞，大人土压夫人。断曰：空亡事不长，阴人多乖张，求财无利益，行人有灾殃，失物寻不见，官事主刑伤，病人逢暗鬼，解禳保安康。",
];

/// 十二时辰名称，下标即时辰序号（子=0 … 亥=11）
const HOUR_NAMES: [&str; 12] = [
    "子时(23-01)",
    "丑时(01-03)",
    "寅时(03-05)",
    "卯时(05-07)",
    "辰时(07-09)",
    "巳时(09-11)",
    "午时(11-13)",
    "未时(13-15)",
    "申时(15-17)",
    "酉时(17-19)",
    "戌时(19-21)",
    "亥时(21-23)",
];

// === 周易六十四卦 ===

/// 六爻阴阳（true=阳爻）→ 文王卦序卦（1–64）。
/// `yao[0]` 是初爻（最下），`yao[5]` 是上爻；每三爻组成一个经卦，
/// 伏羲二进制中上线为 bit0、下线为 bit2，与固件 `get_hexagram_id()` 一致。
fn hexagram_id(yao: &[bool; 6]) -> usize {
    let bit = |v: bool, s: u32| u8::from(v) << s;
    let lower = bit(yao[2], 0) | bit(yao[1], 1) | bit(yao[0], 2);
    let upper = bit(yao[5], 0) | bit(yao[4], 1) | bit(yao[3], 2);
    binary_to_index[((upper << 3) | lower) as usize] as usize
}

/// 摇卦：随机生成六爻并按所选分类给出断语。`category` 为
/// [`CATEGORIES`] 之一，非法值返回 None。
pub fn iching_cast(category: &str) -> Option<Value> {
    let idx = CATEGORIES.iter().position(|c| *c == category)?;
    let mut yao = [false; 6];
    for line in &mut yao {
        *line = random();
    }
    let id = hexagram_id(&yao);
    let h = &ICHING[id];
    Some(json!({
        "hexagram": {
            "number": id + 1,
            "name": h.name,
            "gua_ci": h.gua_ci,
            "xiang": h.xiang,
            "daxiang": h.daxiang,
        },
        "category": CATEGORIES[idx],
        "advice": h.advice(idx),
        "yao": yao.iter().map(|&y| u8::from(y)).collect::<Vec<u8>>(),
    }))
}

// === 小六壬 ===

/// 时钟小时（0–23）→ 时辰序号（子=0 … 亥=11），与固件一致
fn hour_index(hour: u32) -> usize {
    (hour as usize) / 2
}

/// 以当前时间起课：农历月/日/时辰三步取模落六神。
/// 时间或换算失败时返回 None。
pub fn liuren_cast() -> Option<Value> {
    let now = Local::now();
    let solar = SolarDate::from_ymd(now.year() as u16, now.month() as u8, now.day() as u8).ok()?;
    let lunisolar = LunisolarDate::from_solar_date(solar).ok()?;
    let month = lunisolar.to_lunar_month().to_u8() as usize;
    let day = lunisolar.to_lunar_day().to_u8() as usize;
    let hour = hour_index(now.hour());

    let pos = liuren_position(month, day, hour);
    let (name, meaning) = LIUREN_POS[pos];
    let leap = lunisolar.to_lunar_month().is_leap_month();
    let lunar_text = if leap {
        format!("农历闰{month}月{day}日")
    } else {
        format!("农历{month}月{day}日")
    };

    Some(json!({
        "lunar": lunar_text,
        "hour": HOUR_NAMES[hour],
        "position": name,
        "meaning": meaning,
        "judgment": LIUREN_JUDGMENTS[pos],
    }))
}

/// 小六壬核心：从大安起数月、月位起数日、日位起数时，各对 6 取模。
/// 与固件 `liuren_calculate()` 同构，独立出来便于测试。
fn liuren_position(month: usize, day: usize, hour: usize) -> usize {
    let pos = (month - 1) % 6;
    let pos = (pos + day - 1) % 6;
    (pos + hour) % 6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_to_index_is_a_permutation() {
        let mut seen = [false; 64];
        for &v in binary_to_index.iter() {
            assert!((v as usize) < 64, "index out of range: {v}");
            assert!(!seen[v as usize], "duplicate value: {v}");
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|&s| s), "not a permutation of 0..64");
    }

    #[test]
    fn all_yang_is_hexagram_1_qian() {
        assert_eq!(hexagram_id(&[true; 6]), 0); // 第 1 卦 乾为天
    }

    #[test]
    fn all_yin_is_hexagram_2_kun() {
        assert_eq!(hexagram_id(&[false; 6]), 1); // 第 2 卦 坤为地
    }

    /// 固件 `get_hexagram_id()` 的位序注释：s_yao[0]=初爻在下，
    /// 水雷屯（第 3 卦）= 上坎(水 101)下震(雷 100)。
    #[test]
    fn water_thunder_is_hexagram_3_zhun() {
        // 震 = 初爻阳、二爻阴、三爻阴（自下而上）；坎 = 四爻阴、五爻阳、上爻阴
        let yao = [true, false, false, false, true, false];
        assert_eq!(hexagram_id(&yao), 2); // 第 3 卦 水雷屯
    }

    #[test]
    fn iching_cast_rejects_unknown_category() {
        assert!(iching_cast("转运").is_none());
    }

    #[test]
    fn iching_cast_returns_all_categories() {
        for cat in CATEGORIES {
            let v = iching_cast(cat).expect("valid category");
            assert_eq!(v["category"], cat);
            let yao: Vec<u8> = serde_json::from_value(v["yao"].clone()).unwrap();
            assert_eq!(yao.len(), 6);
            assert!((1..=64).contains(&v["hexagram"]["number"].as_u64().unwrap()));
        }
    }

    /// 手推：从大安起，正月落大安(0)、二月留连(1)、三月速喜(2)、
    /// 四月赤口(3)、五月小吉(4)、六月空亡(5)、七月又回大安(0)；
    /// 月位起数日、日位起数时同理。
    #[test]
    fn liuren_positions_by_hand() {
        assert_eq!(liuren_position(1, 1, 0), 0); // 正月初一子时 → 大安
        assert_eq!(liuren_position(1, 1, 1), 1); // 时辰进一位 → 留连
        assert_eq!(liuren_position(2, 1, 0), 1); // 二月初一子时 → 留连
        assert_eq!(liuren_position(7, 1, 0), 0); // 七月回大安
        // 端午午时：五月落小吉(4)，初五数到速喜(2)，午时仍落速喜(2)
        assert_eq!(liuren_position(5, 5, 6), 2);
    }

    /// 固件算错的用例：2025-07-25 是闰六月初一
    /// （chinese-lunisolar-calendar 的历表覆盖此日期）。
    #[test]
    fn liuren_cast_handles_2025_leap_month() {
        let solar = SolarDate::from_ymd(2025, 7, 25).unwrap();
        let l = LunisolarDate::from_solar_date(solar).unwrap();
        let m = l.to_lunar_month();
        assert!(m.is_leap_month());
        assert_eq!(m.to_u8(), 6);
        assert_eq!(l.to_lunar_day().to_u8(), 1);
    }

    #[test]
    fn liuren_cast_returns_complete_result() {
        let v = liuren_cast().expect("lunisolar conversion should work");
        assert!(v["lunar"].as_str().unwrap().starts_with("农历"));
        assert!(v["hour"].as_str().unwrap().ends_with(')'));
        let pos = v["position"].as_str().unwrap();
        assert!(LIUREN_POS.iter().any(|&(n, _)| n == pos));
    }

    /// 时辰划分与固件 `hour_index_map` 一致：0,0,1,1,…,11,11，
    /// 即 23 点归当日亥时（不用晚子时归次日的分法）。
    #[test]
    fn hour_index_matches_zodiac_hours() {
        assert_eq!(hour_index(0), 0); // 子
        assert_eq!(hour_index(1), 0); // 子
        assert_eq!(hour_index(2), 1); // 丑
        assert_eq!(hour_index(12), 6); // 午
        assert_eq!(hour_index(21), 10); // 亥
        assert_eq!(hour_index(23), 11); // 亥（固件分法：23 点仍属当日亥时）
    }
}
