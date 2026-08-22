// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! フォント資産 ([`FontAssets`]) と、 インストール済みフォント family の列挙
//! (M14 Phase 121 / daw_01 #096)。
//!
//! Text クリップのフォントピッカー等で、 `GlyphArea.font_family` に渡せる family 名の集合を
//! 取得するための free 関数。 GPU も live [`FontSystem`](glyphon::FontSystem) も要求しないので、
//! GUI の background thread から呼べる。

use std::collections::BTreeSet;

use glyphon::{FontSystem, SwashCache};

/// **CPU 側**のフォント資産 (font DB + glyph raster cache)。
///
/// GPU デバイスに一切依存しないので、 device lost (= スリープ復帰で GPU が電源断される等、
/// daw_01 r.md #42) で GPU 資産を丸ごと作り直しても **これは作り直さない**。
/// `FontSystem::new()` は OS のフォントディレクトリ全走査 (~20-860ms、 [`available_font_families`]
/// のコスト節参照) を伴うため、 再生成のたびに走らせるのは論外。
///
/// 同時に、 これは daw-ui 全体で **font DB の Single Source of Truth** でもある。
/// 以前は `GlyphPipeline` が `FontSystem` / `SwashCache` を内包しており、 base pass 用と
/// popup pass 用で 2 インスタンス生成 = システムフォント全走査を 2 回やっていた。
/// CPU 資産をこの型に括り出したことで、 その二重ロードが構造的に消えている
/// (`Renderer` / `OffscreenRenderer` が 1 個だけ持ち、 各 pipeline へ `&mut` で貸す)。
pub struct FontAssets {
    pub font_system: FontSystem,
    pub swash_cache: SwashCache,
}

impl FontAssets {
    /// システムフォントを走査して構築する (**重い**、 renderer 1 個につき 1 回)。
    #[must_use]
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
        }
    }

    /// glyphon の `prepare` は `&mut FontSystem` と `&mut SwashCache` を別引数で取るので、
    /// disjoint-field borrow をまとめて取り出す accessor。
    pub fn split(&mut self) -> (&mut FontSystem, &mut SwashCache) {
        (&mut self.font_system, &mut self.swash_cache)
    }
}

impl Default for FontAssets {
    fn default() -> Self {
        Self::new()
    }
}

/// この環境にインストールされているフォント family 名を、 **ソート済み・重複排除** して返す。
///
/// 返る各名前は `GlyphArea { font_family: Some(name.into()), .. }` (= glyphon の
/// `Family::Name`) で**必ず解決できる**。 renderer の [`FontSystem`](glyphon::FontSystem) が内部で
/// 使うのと **同じ `fontdb` バージョン**・同じ `load_system_fonts()` を呼ぶため、 ここで列挙した
/// 集合は実描画で解決される集合と一致する。
///
/// # 名前の選び方
/// 各 face の `families` リスト先頭 (= `fontdb` 規約で英語 US 名、 無ければ最初に得られる名前) を
/// 採用する。 これは `Family::Name` の照合に使われる正準名であり、 日本語名しか持たないフォントは
/// その日本語名が返る。
///
/// # コスト
/// 内部で OS のフォントディレクトリ全走査 (`load_system_fonts`) を行うため初回 ~20-860ms 程度。
/// **毎フレーム呼ばない**こと。 結果は caller 側でキャッシュするとよい (フォント追加/削除を
/// 反映したいときだけ再呼び出し)。
#[must_use]
pub fn available_font_families() -> Vec<String> {
    // glyphon::fontdb は cosmic-text 経由の re-export で、 renderer の FontSystem と同一バージョン。
    // 別 `fontdb` を直接依存に足すと version skew で family 解決がズレうるので、 必ずこの経路を使う。
    let mut db = glyphon::fontdb::Database::new();
    db.load_system_fonts();

    // BTreeSet で挿入と同時にソート + 重複排除 (複数 face が同 family を共有: Regular/Bold/Italic 等)。
    let mut set: BTreeSet<String> = BTreeSet::new();
    for face in db.faces() {
        if let Some((name, _lang)) = face.families.first() {
            set.insert(name.clone());
        }
    }
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dev 環境 (Windows / macOS / Linux のいずれも system font 必ず存在) で非空、
    /// かつ戻り値が **ソート済み・重複なし** であること。
    #[test]
    fn available_font_families_sorted_deduped_nonempty() {
        let families = available_font_families();
        assert!(
            !families.is_empty(),
            "system fonts が 1 つも列挙できないのは異常 (load_system_fonts 失敗?)"
        );
        // ソート済み (BTreeSet 由来なので不変条件のはずだが回帰固定)。
        let mut sorted = families.clone();
        sorted.sort();
        assert_eq!(families, sorted, "available_font_families はソート済みで返すべき");
        // 重複なし。
        let mut deduped = families.clone();
        deduped.dedup();
        assert_eq!(families.len(), deduped.len(), "重複 family を含んではいけない");
    }
}
