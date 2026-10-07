//! CJK 字体加载：修复中文（及日/韩文）在 egui 中显示为方块（tofu）的问题。
//!
//! 背景：egui 内置字体（Ubuntu-Light / NotoEmoji 等）**不含 CJK 字形**，
//! 在 Linux（deepin 等）默认构建下所有中文渲染为 `□`。修复方式：启动时在
//! 系统字体目录中查找一款 CJK 字体，把它追加到 `Proportional` / `Monospace`
//! / `Button` 等家族的**字形回退链**末尾（egui 对家族内逐个字体查找缺失
//! 字形，因此拉丁字形仍走内置字体，不影响英文观感）。
//!
//! 查找顺序（命中即停）：
//! 1. 常见 CJK 字体的固定路径（deepin/Debian/Arch/Fedora 布局 + Windows/macOS）；
//! 2. 扫描字体目录，按文件名匹配 CJK 关键字（`cjk` / `wqy` / `uming` / `msyh` 等）。
//!
//! `.ttc` 字体集合取第 0 个 face（Noto Sans CJK 的任意 regional face 都覆盖
//! 简繁常用汉字；字形风格略有差异但不影响可读性）。
//!
//! 找不到任何 CJK 字体时返回 `false`，界面照常启动（拉丁文本正常，CJK 仍为
//! 方块）——与修复前行为一致，不 panic。

use std::path::PathBuf;

use eframe::egui;

/// 常见 CJK 字体候选路径（按优先级）。
const CJK_FONT_CANDIDATES: &[&str] = &[
    // Linux: Noto Sans CJK（deepin/Debian/Ubuntu/Fedora 常见布局）
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-sans-cjk-fonts/NotoSansCJK-Regular.ttc",
    // 简体单文件变体
    "/usr/share/fonts/truetype/noto/NotoSansSC-Regular.otf",
    "/usr/share/fonts/truetype/winfonts/NotoSansSC-VF.ttf",
    // 文泉驿（deepin/老发行版常见）
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    // Droid 兜底（Android 系/部分精简发行版）
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
    "/usr/share/fonts/truetype/droid/DroidSansFallback.ttf",
    // 未楷/未黑（传统发行版）
    "/usr/share/fonts/truetype/arphic/uming.ttc",
    "/usr/share/fonts/truetype/arphic/ukai.ttc",
    // Windows
    "C:\\Windows\\Fonts\\msyh.ttc",
    "C:\\Windows\\Fonts\\simsun.ttc",
    // macOS
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
];

/// 文件名中包含这些关键字（小写匹配）即视为 CJK 字体（目录扫描兜底用）。
const CJK_NAME_HINTS: &[&str] = &[
    "cjk", "notosanssc", "notoserifsc", "wqy", "microhei", "zenhei", "uming", "ukai",
    "droidsansfallback", "msyh", "simsun", "simhei", "pingfang", "sourcehansans",
];

/// 在候选路径 + 字体目录扫描中找到一款可用的 CJK 字体。
pub fn find_cjk_font() -> Option<PathBuf> {
    // 1) 固定候选
    for p in CJK_FONT_CANDIDATES {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    // 2) 目录扫描（XDG 数据目录 + 用户字体目录）
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        roots.push(PathBuf::from(xdg).join("fonts"));
    } else if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/fonts"));
        roots.push(home.join(".fonts"));
    }
    roots.push(PathBuf::from("/usr/share/fonts"));
    roots.push(PathBuf::from("/usr/local/share/fonts"));
    for root in roots {
        if let Some(hit) = scan_dir(&root, 0) {
            return Some(hit);
        }
    }
    None
}

/// 递归扫描字体目录（限深 4 层，避免在大目录上拖慢启动）。
fn scan_dir(dir: &std::path::Path, depth: usize) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    let rd = std::fs::read_dir(dir).ok()?;
    // 先扫本层文件，再下钻子目录（浅层优先命中更常见的布局）
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            subdirs.push(p);
            continue;
        }
        if !matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("ttf") | Some("otf") | Some("ttc")
        ) {
            continue;
        }
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if CJK_NAME_HINTS.iter().any(|h| name.contains(h)) {
            return Some(p);
        }
    }
    for d in subdirs {
        if let Some(hit) = scan_dir(&d, depth + 1) {
            return Some(hit);
        }
    }
    None
}

/// 把找到的 CJK 字体安装进 egui（所有文本家族的字形回退链末尾）。
///
/// 返回是否成功安装（用于启动日志提示；失败只是没有 CJK 字形，不是错误）。
pub fn install_cjk_fonts(ctx: &egui::Context) -> bool {
    install_cjk_fonts_from(ctx, find_cjk_font().as_deref())
}

/// 指定字体路径的安装（测试用，避免依赖运行环境的字体集）。
pub fn install_cjk_fonts_from(ctx: &egui::Context, font: Option<&std::path::Path>) -> bool {
    let Some(path) = font else {
        return false;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    // egui 的 FontData 支持 ttc 集合（index=0 取第一个 face）。
    let data = egui::FontData::from_owned(bytes);
    let mut fonts = egui::FontDefinitions::default();
    // 已有同名则覆盖（幂等：重复调用不会叠加回退项）
    fonts.font_data.insert("rdep-cjk".into(), data);
    for family in [
        egui::FontFamily::Proportional,
        egui::FontFamily::Monospace,
    ] {
        let list = fonts.families.entry(family).or_default();
        if !list.iter().any(|n| n == "rdep-cjk") {
            list.push("rdep-cjk".into());
        }
    }
    ctx.set_fonts(fonts);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机（测试环境/deepin）应能找到一款 CJK 字体；若无则提示为 None 而非 panic。
    #[test]
    fn find_cjk_font_smoke() {
        if let Some(p) = find_cjk_font() {
            assert!(p.is_file(), "found path must exist: {}", p.display());
        }
    }

    /// 目录扫描：临时目录内按关键字命中目标字体文件。
    #[test]
    fn scan_dir_finds_hint_named_font() {
        let base = std::env::temp_dir().join(format!("rdep-fonts-{}", std::process::id()));
        let sub = base.join("truetype").join("wqy");
        std::fs::create_dir_all(&sub).unwrap();
        let target = sub.join("wqy-microhei.ttc");
        std::fs::write(&target, b"fake").unwrap();
        let hit = scan_dir(&base, 0).expect("should find font by name hint");
        assert_eq!(hit, target);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 非字体扩展名不命中；空目录返回 None。
    #[test]
    fn scan_dir_ignores_non_fonts() {
        let base = std::env::temp_dir().join(format!("rdep-fonts-empty-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("wqy-microhei.txt"), b"x").unwrap();
        assert!(scan_dir(&base, 0).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
