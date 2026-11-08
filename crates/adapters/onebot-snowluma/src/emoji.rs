//! QQ 系统表情 id → 人话。
//!
//! 平台只给编号（`5`、`424`），投递里直接甩编号等于没给。这里维护一份最小映射表：
//! 认得的给名字，认不得的保留编号并标「未知」——既让人看懂，也让以后补表时能
//! 从投递里直接看到哪个 id 还没收录。

/// 经典 QQ 系统表情。只收编号稳定、名字没有歧义的那一段；后面的表情不同客户端
/// 编号有分歧，宁可不猜。
const SYSTEM_FACES: &[(&str, &str)] = &[
    ("0", "惊讶"),
    ("1", "撇嘴"),
    ("2", "色"),
    ("3", "发呆"),
    ("4", "得意"),
    ("5", "流泪"),
    ("6", "害羞"),
    ("7", "闭嘴"),
    ("8", "睡"),
    ("9", "大哭"),
    ("10", "尴尬"),
    ("11", "发怒"),
    ("12", "调皮"),
    ("13", "呲牙"),
    ("14", "微笑"),
    ("15", "难过"),
    ("16", "酷"),
    ("17", "爱心"),
    ("18", "白眼"),
    ("19", "鼓掌"),
    ("20", "晕"),
    ("21", "恐惧"),
    ("22", "流汗"),
    ("23", "憨笑"),
    ("24", "傲慢"),
    ("25", "饥饿"),
    ("26", "困"),
    ("27", "咒骂"),
    ("28", "疑问"),
    ("29", "嘘"),
    ("30", "打击"),
    ("31", "再见"),
    ("32", "轰炸"),
    ("33", "抠鼻"),
];

/// 表情 id → 给人看的描述。
///
/// 认得的写成 `名字（id）`，认不得的写成 `表情 id（未知）`。自定义表情的 id 是
/// 32 位哈希，同样归到「未知」——不编名字，但编号留住。
pub fn describe_face(id: &str) -> String {
    if id.is_empty() {
        return "表情".to_owned();
    }
    match SYSTEM_FACES
        .iter()
        .find_map(|(known, name)| (*known == id).then_some(*name))
    {
        Some(name) => format!("{name}（{id}）"),
        None => format!("表情 {id}（未知）"),
    }
}

#[cfg(test)]
mod tests {
    use super::describe_face;

    #[test]
    fn known_ids_get_a_name_and_keep_the_number() {
        assert_eq!(describe_face("5"), "流泪（5）");
        assert_eq!(describe_face("14"), "微笑（14）");
    }

    #[test]
    fn unknown_ids_keep_the_number_and_say_unknown() {
        assert_eq!(describe_face("424"), "表情 424（未知）");
    }

    #[test]
    fn custom_emoji_hashes_are_still_unknown_but_readable() {
        assert_eq!(
            describe_face("0123456789abcdef0123456789abcdef"),
            "表情 0123456789abcdef0123456789abcdef（未知）"
        );
    }

    #[test]
    fn empty_id_degrades_to_a_plain_word() {
        assert_eq!(describe_face(""), "表情");
    }
}
