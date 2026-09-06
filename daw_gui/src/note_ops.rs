//! note_ops — MIDI ノート集合に対するピュアな編集ロジック (複製 / コピー / 重なり解決 / index 写像)。
//!
//! `app_types.rs` から切り出し (不変条件 9 のサイズ budget)。`AppData` に依存せず
//! `MidiContent` / `Note` だけを触るので、handler と `midi_import` の両方から使う。
use common::model::{MidiContent, Note};

/// 選択ノートを複製して `notes` 末尾に追加するピュアロジック (D キー複製の core)。
/// `selected` の各 index のノートを clone し、`offset` 拍ぶん後ろへずらして append する。
///
/// `offset` は**選択範囲の長さ**を呼び出し側が渡す (`docs/plan_range_selection.md` §6)。
/// ノートの外接 span から計算してはいけない — 裏拍のパターンは頭と尻に空白があり、
/// 外接 span で送ると 1 回ごとに詰まってグリッドから外れる。
///
/// 元ノートは不変。戻り値は複製ノートの新 index (= 複製後の選択)。
/// 選択が空 / 該当 index 無し / `offset <= 0` なら `notes` 不変で空 Vec を返す。
pub(crate) fn duplicate_notes_into(
    content: &mut MidiContent,
    selected: &[u32],
    offset: f64,
) -> Vec<u32> {
    let mut clones: Vec<Note> = selected
        .iter()
        .filter_map(|&idx| content.notes.get(idx as usize).cloned())
        .collect();
    if clones.is_empty() || offset <= 0.0 {
        return Vec::new();
    }
    for n in &mut clones {
        n.start_beat += offset;
        // clone は元 note の `id` も複製する。 per-content 一意 note id 不変条件
        // (invariant #1、 piano_roll が selection/hit-test/drag/edit に note.id を使う)
        // を守るため新規採番する。 さもないと複製ノートを選択/ドラッグすると元と
        // 両方に作用する (M4 = duplicate_audio_editor_event の sibling)。
        n.id = content.alloc_note_id();
    }
    let base = content.notes.len() as u32;
    let count = clones.len() as u32;
    content.notes.append(&mut clones);
    (base..base + count).collect()
}

/// gui_01 #054 (Ctrl+drag コピー) の core。`entries` = [(source note index,
/// new_start_beat, new_pitch)]。各 source を clone し start_beat/pitch を指定値にして
/// `notes` 末尾へ追加。元は不変。戻り値は複製の新 index。該当 index 無しなら不変で空 Vec。
pub(crate) fn copy_notes_into(content: &mut MidiContent, entries: &[(u32, f64, u8)]) -> Vec<u32> {
    let mut clones: Vec<Note> = Vec::new();
    for &(idx, new_beat, new_pitch) in entries {
        if let Some(src) = content.notes.get(idx as usize) {
            let mut c = src.clone();
            c.start_beat = new_beat.max(0.0);
            c.pitch = new_pitch;
            clones.push(c);
        }
    }
    if clones.is_empty() {
        return Vec::new();
    }
    // clone は元 note の `id` を複製するので新規採番する (duplicate_notes_into と
    // 同じ per-content 一意 id 不変条件、 invariant #1)。
    for c in &mut clones {
        c.id = content.alloc_note_id();
    }
    let base = content.notes.len() as u32;
    let count = clones.len() as u32;
    content.notes.append(&mut clones);
    (base..base + count).collect()
}

/// 同一ピッチの MIDI ノートが時間的に重ならない不変条件を強制する
/// (Bitwig / Ableton 流、`docs/plan_fixme_83_note_overlap.md`)。
///
/// `winners` = 直前に追加 / 移動 / サイズ変更 / コピーされたノートの index (= 衝突時に
/// 勝つ側)。同一ピッチで winner と重なる loser を **last-note-wins** で解消する:
/// - 完全被覆 → loser 削除
/// - loser が winner より前に始まる → loser 末尾を winner 開始でトリム
///   (末尾重なり + 中央挿入 = truncate-not-split、自動分割しない)
/// - loser 先頭が winner に覆われ後半が残る → loser 開始を winner 終端へ前送りし後半を残す
///   (REAPER 流の非破壊的挙動、user 採用)
///
/// winner 同士の重なり (時間 / ピッチ量子化・glue で発生し得る) は pitch ごとに start 昇順で
/// 「後から始まる方が勝ち、前のノート末尾をトリム」で解消する (move / copy 等の並進操作では
/// winner 群は重ならないので no-op)。**異なるピッチは一切触らない** (= 和音は自由)。
///
/// 削除で index がずれるため、戻り値は古い index → 新 index の remap 表 (削除は `None`)。
/// 削除は降順 `Vec::remove` で行う (`delete_selected_notes` と同 idiom)。caller は
/// [`remap_indices`] で `selected_notes` / 新規 winner id を写し替える。
pub(crate) fn resolve_note_overlaps(notes: &mut Vec<Note>, winners: &[u32]) -> Vec<Option<u32>> {
    const EPS: f64 = 1e-9;
    let n = notes.len();
    // winner index を範囲内・重複除去して正規化 (入力順は保持)。
    let mut is_winner = vec![false; n];
    let mut winner_order: Vec<usize> = Vec::new();
    for &w in winners {
        let w = w as usize;
        if w < n && !is_winner[w] {
            is_winner[w] = true;
            winner_order.push(w);
        }
    }
    let mut deleted = vec![false; n];

    // ---- Phase B: winner 同士の重なり解消 (pitch ごとに start 昇順、後勝ち) ----
    {
        let mut by_pitch: std::collections::HashMap<u8, Vec<usize>> =
            std::collections::HashMap::new();
        for &w in &winner_order {
            by_pitch.entry(notes[w].pitch).or_default().push(w);
        }
        for group in by_pitch.values_mut() {
            if group.len() < 2 {
                continue;
            }
            group.sort_by(|&a, &b| {
                notes[a]
                    .start_beat
                    .total_cmp(&notes[b].start_beat)
                    .then(a.cmp(&b))
            });
            for i in 0..group.len() - 1 {
                let a = group[i];
                let c = group[i + 1];
                if deleted[a] {
                    continue;
                }
                let a_end = notes[a].start_beat + notes[a].duration_beats;
                let c_start = notes[c].start_beat;
                if a_end > c_start + EPS {
                    let new_dur = c_start - notes[a].start_beat;
                    if new_dur <= EPS {
                        deleted[a] = true;
                    } else {
                        notes[a].duration_beats = new_dur;
                    }
                }
            }
        }
    }

    // ---- Phase A: 各 winner が同一ピッチの loser をトリム / 削除 ----
    for &w in &winner_order {
        if deleted[w] {
            continue;
        }
        let ws = notes[w].start_beat;
        let we = ws + notes[w].duration_beats;
        if we <= ws + EPS {
            continue;
        }
        let p = notes[w].pitch;
        for b in 0..n {
            if b == w || is_winner[b] || deleted[b] || notes[b].pitch != p {
                continue;
            }
            let bs = notes[b].start_beat;
            let be = bs + notes[b].duration_beats;
            // 重なり無し (隣接 be == ws / bs == we は許容)。
            if be <= ws + EPS || bs >= we - EPS {
                continue;
            }
            if ws <= bs + EPS && be <= we + EPS {
                // 完全被覆 → 削除。
                deleted[b] = true;
            } else if bs < ws - EPS {
                // loser が winner より前に始まる → 末尾を winner 開始でトリム
                // (末尾重なり + 中央挿入 = truncate-not-split)。
                let new_dur = ws - bs;
                if new_dur <= EPS {
                    deleted[b] = true;
                } else {
                    notes[b].duration_beats = new_dur;
                }
            } else {
                // loser 先頭が winner に覆われ後半が we を超える → 開始を we へ前送り
                // して後半を残す (REAPER 流、user 採用)。
                let new_dur = be - we;
                if new_dur <= EPS {
                    deleted[b] = true;
                } else {
                    notes[b].start_beat = we;
                    notes[b].duration_beats = new_dur;
                }
            }
        }
    }

    // ---- 削除を適用 + remap 表を構築 ----
    let mut remap: Vec<Option<u32>> = vec![None; n];
    let mut deleted_before = 0u32;
    for i in 0..n {
        if deleted[i] {
            deleted_before += 1;
        } else {
            remap[i] = Some(i as u32 - deleted_before);
        }
    }
    for i in (0..n).rev() {
        if deleted[i] {
            notes.remove(i);
        }
    }
    remap
}

/// [`resolve_note_overlaps`] の remap 表で古い index 列を新 index 列へ写す
/// (削除されたものは除外)。selected_notes / 新規 winner id の付け替えに使う。
pub(crate) fn remap_indices(remap: &[Option<u32>], idxs: &[u32]) -> Vec<u32> {
    idxs.iter()
        .filter_map(|&i| remap.get(i as usize).copied().flatten())
        .collect()
}
