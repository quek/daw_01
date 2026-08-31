//! 統一クリップボード envelope。
//!
//! cut/copy/paste の全対象 (ノート / オートメーションの点 / オーディオイベント /
//! クリップ / トラック) を 1 つの型タグ付き envelope に入れ、OS クリップボードへ
//! JSON text として書く (`Ui::set_clipboard_text` / `Ui::take_clipboard_paste`)。
//! paste 側は trial-decode をやめ、`ClipboardPayload` の variant で分岐する。
//!
//! `source_project_id` は copy 元の `Song.project_id`。paste 時に現在の song と
//! 一致すれば clip/track は **リンク共有** (content_id 流用)、不一致なら inline
//! payload から **独立コピー** (新 content_id 採番) する。
//!
//! 外部 (他アプリ / 改竄) clipboard text を信用しないため、deserialize 後に
//! 値域を sanitize する (NaN/Inf/範囲外を model に入れない)。

use common::model::{
    AudioEvent, AutomationCurve, ClipContent, ContentId, Note, Track,
};
use serde::{Deserialize, Serialize};

/// daw_01 clipboard を他アプリ text と区別するマーカー兼 format version。
/// 形式を破壊変更したら version を上げる (古い clipboard は magic 不一致で no-op)。
///
/// r.md #71 (プラグインのコピー / 移動) で `ClipboardPayload::Devices` を足したが
/// **version は据え置く**。 serde の externally-tagged enum に variant を足すと
/// 新 build が書いた clipboard を旧 build は deserialize できないが、
/// [`ClipboardEnvelope::from_json`] は `serde_json::from_str(..).ok()?` なので
/// **decode 失敗は `None` = 静かな no-op** に落ちる (magic 一致で誤爆はしない)。
/// version を上げると逆に「旧 build が書いた clipboard を新 build が読めない」を
/// 新規に作ってしまう。
pub const CLIPBOARD_MAGIC: &str = "daw_01.clipboard.v1";

/// r.md #71 (プラグインのコピー / 移動): 1 回のコピーで OS クリップボードへ載せる
/// blob の総量上限 (base64 前の生バイト)。
///
/// `PluginInstance.state` / `ara_archive` は `#[serde(with = "base64_opt")]` なので
/// **base64 テキストとして OS クリップボードへ流れる**。 サンプラー系の state は
/// 数十 MB になり得るが、 OS クリップボードはテキストの通り道であって、
/// そこを運ぶ場所ではない。 **超える分は運ばずに落とし、 status_message で何を
/// 落としたか明示する** — 黙って切ると「貼ったら音色が違う」の原因が見えなくなる。
/// 大きい state ごと運びたいときはドラッグ&ドロップ / 複製 (どちらもプロセス内で
/// `Arc` を clone するだけ) を使う。
pub const CLIPBOARD_BLOB_BUDGET: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardEnvelope {
    pub magic: String,
    /// copy 元の `Song.project_id`。同一なら clip/track paste はリンク共有。
    pub source_project_id: u64,
    pub payload: ClipboardPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipboardPayload {
    Notes(Vec<Note>),
    AutomationPoints(Vec<CopiedPoint>),
    AudioEvents(Vec<AudioEvent>),
    Clips(Vec<ClipCopy>),
    AutomationClips(Vec<AutomationClipCopy>),
    Tracks(TracksCopy),
    /// r.md #87 (クリップランチャー): ランチャーのセル。
    /// アレンジのクリップ (`Clips`) とは **貼り先の座標系が違う** ので別 variant に
    /// する — アレンジは (トラック, 拍)、ランチャーは (行, 列) で、同じ payload に
    /// 押し込むと「拍」に列番号を入れる類の嘘が要る。
    LauncherCells(Vec<LauncherCellCopy>),
    /// r.md #71 (プラグインのコピー / 移動): チェーンから選んだプラグイン。
    Devices(Vec<DeviceCopy>),
}

/// 正規化済み device。`order` は選択群内の相対順 (上から 0,1,2...) で、貼り付けで
/// 相対順を保つ。`device.id` は **0 に落として運ぶ** (貼り先で必ず新採番する)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCopy {
    pub order: usize,
    /// コピー元の所属トラック。貼り付け先が同じなら ARA アーカイブを引き継ぐ
    /// (別トラックなら捨てて解析し直す — `handler/sync.rs` の persistent_id が
    /// 元トラックのクリップを指すため)。
    pub source_track: u32,
    pub device: common::model::PluginInstance,
}

/// 正規化済みオートメーション点。`value_norm` は target 非依存の 0..=1 normalized
/// (paste 先 lane の値域へ `norm_to_plain` で復元)。`time_beat` は選択群の最早を
/// 0 とした相対拍。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopiedPoint {
    pub time_beat: f64,
    pub value_norm: f32,
    pub curve: AutomationCurve,
}

/// 正規化済みクリップ。`track_offset` は選択群の最上段トラックを 0 とした相対
/// トラック index、`start_beat` は最早クリップ start を 0 とした相対拍。
/// `content` は cross-project 独立復元用に inline、`content_id` は同一プロジェクト
/// リンク共有用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipCopy {
    pub track_offset: i64,
    pub start_beat: f64,
    pub length_beats: f64,
    pub color: Option<[f32; 3]>,
    // `auto_lipsync` (口パクの自動生成 clip か) は **運ばない**。 clipboard は
    // ユーザーが明示的に置き直す内容を運ぶものなので、 paste した clip は
    // 派生データではなくユーザーの持ち物になる。 運んでしまうと口 track に
    // auto clip が 2 本並び、「高々 1 本」 (r.md #17) の不変条件が壊れて
    // **開くたびに畳み直し = `*`** が付いた (r.md #9)。
    /// clip-level mute。 paste 先 clip へ引き継ぐ。 旧 clipboard JSON
    /// との互換のため serde default (`false`)。
    #[serde(default)]
    pub muted: bool,
    /// source プロジェクトでの content_id (同一プロジェクト paste でリンク共有)。
    pub content_id: ContentId,
    /// r.md #44: clip が content のどこを見せていたか (`Clip::content_offset_beats`)。
    /// これを運ばないと、左端を trim した clip を copy&paste しただけで窓が先頭へ
    /// 戻ってしまう。旧 clipboard JSON との互換のため serde default (`0.0`)。
    #[serde(default)]
    pub content_offset_beats: f64,
    /// content payload (別プロジェクト paste で独立復元)。
    pub content: ClipContent,
    /// 共有名 (`Song.clip_content_names` 由来)。
    pub name: Option<String>,
    /// per-clip VOICEVOX 声。 paste 先 clip へ引き継ぐ。 旧 clipboard
    /// JSON との互換のため serde default (0 / 空)。
    #[serde(default)]
    pub speaker_id: u32,
    #[serde(default)]
    pub singer_name: String,
    #[serde(default)]
    pub style_name: String,
    /// (talk) per-clip 読み上げスケール。 paste 先 clip へ引き継ぐ。 旧 clipboard
    /// JSON との互換のため serde default (`None`)。
    #[serde(default)]
    pub talk: Option<common::model::TalkParams>,
}

/// 正規化済みオートメーションクリップ (= lane 内の 1 curve clip)。`start_beat` は
/// 選択群の最早クリップ start を 0 とした相対拍、`length_beats` は clip 長。`points` は
/// **clip-local 時間 + target 非依存 normalized 値** (`CopiedPoint` を流用、 paste 先 lane の
/// `norm_to_plain` で復元) なので、 異なる target の lane に貼っても curve の形を保てる
/// (= automation point copy と同じ Bitwig 流)。
///
/// `source_content_id` は copy 元の `content_id`。 同一 content を共有していた linked clip
/// 群を **paste 後も互いにリンク** させる dedup キーとしてのみ使う (= song の content を
/// そのまま流用はしない。 automation 値は target 依存なので、 リンク復元は常に inline
/// `points` から独立採番する)。 これは MIDI clip paste (`ClipCopy`) が同一プロジェクトで
/// content_id を流用してソースとリンクするのと異なり、 REAPER / Ableton の envelope
/// copy 同様「コピー元から切り離した独立コピー」 を作る方針。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationClipCopy {
    pub start_beat: f64,
    pub length_beats: f64,
    /// copy 元 content_id (linked group の内部リンク保持用 dedup キー)。
    pub source_content_id: ContentId,
    /// clip-local 時間 + normalized 値の curve 点列 (空 = default_value 引きずりの空 clip)。
    pub points: Vec<CopiedPoint>,
    /// 共有名 (`Song.clip_content_names` 由来)。
    pub name: Option<String>,
}

/// r.md #87: ランチャーのセル 1 つ。位置は **選択群の左上を (0, 0) とした相対
/// (行, 列)** で運ぶ (貼り先はポインタ下のセル)。中身はアレンジのクリップと
/// 同じ [`ClipCopy`] / [`AutomationClipCopy`] をそのまま使う — セルは
/// 「アレンジと同じ中身を別の入れ物に置いたもの」 なので、正規化の規則を
/// 二重に持たない。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherCellCopy {
    /// 選択群の最上段行を 0 とした相対行 index。
    pub row_offset: i64,
    /// 選択群の最左列を 0 とした相対列 index。
    pub scene_offset: i64,
    pub cell: LauncherCellPayload,
    /// セルのローンチ設定 (量子化 / モード / ループ / レガート / フォロー)。
    pub launch: common::model::LaunchSettings,
}

/// [`LauncherCellCopy`] の中身。トラック行のセルとオートメーションレーン行の
/// セルで型が違う (それぞれ `Clip` / `AutomationClip`)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LauncherCellPayload {
    Track(ClipCopy),
    Lane(AutomationClipCopy),
}

/// r.md #87: トラックまるごとのコピー 1 回ぶん。`tracks` に加えて **コピー元の
/// 列の並び** (`scenes`) を運ぶ。
///
/// `Track::session_clips` の `scene_id` は **プロジェクトごとの id 空間**
/// (設計正本 §1.1 の「Scene は Song 内で安定 id」) なので、別プロジェクトへ
/// そのまま持ち込むと意味の違う列を指す。貼り先の列を決められるのは
/// 「元で何列目だったか」だけなので、id → 表示 index を解く表をコピー側で
/// 1 度だけ載せる (トラックごとに持たせると同じ表が N 本に複製される)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracksCopy {
    pub tracks: Vec<TrackCopy>,
    /// コピー元 `Song.scenes` の id を **表示順**に並べたもの。`0` は
    /// 「解けない列」の sentinel ([`sanitize_tracks`] が重複を潰すのに使う)。
    #[serde(default)]
    pub scenes: Vec<u32>,
}

/// 正規化済みトラックまるごと。`track` は raw (旧 legacy field は skip_serializing で
/// 落ちる)。`order` は選択群内の相対順 (上から 0,1,2...) で、paste で相対順を保つ。
/// `contents` は track の clips / automation lanes / **ランチャーのセル** が参照する
/// content payload を cross-project 独立復元のため同梱する
/// (数え上げは `Track::all_clips` / `AutomationLane::all_clips` を通すこと)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackCopy {
    pub order: usize,
    pub track: Track,
    pub contents: Vec<ContentEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentEntry {
    pub content_id: ContentId,
    pub content: ClipContent,
    pub name: Option<String>,
}

impl ClipboardEnvelope {
    pub fn new(source_project_id: u64, payload: ClipboardPayload) -> Self {
        Self {
            magic: CLIPBOARD_MAGIC.to_string(),
            source_project_id,
            payload,
        }
    }

    /// envelope を JSON 文字列へ。OS clipboard に書く。
    pub fn to_json(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }

    /// OS clipboard から取った text を envelope として decode。magic 不一致 /
    /// decode 失敗 (= 他アプリの text) は `None` で no-op。
    pub fn from_json(text: &str) -> Option<Self> {
        let env: ClipboardEnvelope = serde_json::from_str(text).ok()?;
        if env.magic != CLIPBOARD_MAGIC {
            return None;
        }
        Some(env)
    }
}

/// 外部 clipboard 由来の `Note` 群を sanitize (pitch 0..=127 / velocity 1..=127 /
/// start・duration が非有限 or 負/0 のものは破棄)。既存 `paste_notes_from_json` の
/// 値域チェックと同一。
pub fn sanitize_notes(notes: Vec<Note>) -> Vec<Note> {
    notes
        .into_iter()
        .filter_map(|mut n| {
            if !n.start_beat.is_finite()
                || n.start_beat < 0.0
                || !n.duration_beats.is_finite()
                || n.duration_beats <= 0.0
            {
                return None;
            }
            n.pitch = n.pitch.min(127);
            n.velocity = n.velocity.clamp(1, 127);
            Some(n)
        })
        .collect()
}

/// 外部 clipboard 由来の `AudioEvent` 群を sanitize (start/length 非有限・負を破棄、
/// source frame の前後関係・各 gain/pan/pitch を clamp)。
pub fn sanitize_audio_events(events: Vec<AudioEvent>) -> Vec<AudioEvent> {
    events
        .into_iter()
        .filter_map(|mut e| {
            if !e.event_start_in_clip_beats.is_finite()
                || e.event_start_in_clip_beats < 0.0
                || !e.event_length_beats.is_finite()
                || e.event_length_beats <= 0.0
            {
                return None;
            }
            if e.source_end_frames < e.source_start_frames {
                return None;
            }
            if !e.gain_db.is_finite() {
                e.gain_db = 0.0;
            }
            e.gain_db = e.gain_db.clamp(-60.0, 24.0);
            e.pan = if e.pan.is_finite() { e.pan.clamp(-1.0, 1.0) } else { 0.0 };
            // 範囲は setter / inspector と同じ定数を引く (r.md #40 以前は
            // setter ±96 / ここ ±48 と割れていて、コピー&ペーストするだけで
            // ピッチが静かに切られていた)。
            e.pitch_semitones = common::model::clamp_semitones(
                e.pitch_semitones,
                common::model::PITCH_SEMITONES_LIMIT,
            );
            e.formant_semitones = common::model::clamp_semitones(
                e.formant_semitones,
                common::model::FORMANT_SEMITONES_LIMIT,
            );
            e.fade_in_beats = if e.fade_in_beats.is_finite() {
                e.fade_in_beats.max(0.0)
            } else {
                0.0
            };
            e.fade_out_beats = if e.fade_out_beats.is_finite() {
                e.fade_out_beats.max(0.0)
            } else {
                0.0
            };
            Some(e)
        })
        .collect()
}

/// 外部 clipboard 由来の `CopiedPoint` 群を sanitize (time 非有限・負を破棄、
/// value_norm を 0..=1 に clamp)。
pub fn sanitize_points(points: Vec<CopiedPoint>) -> Vec<CopiedPoint> {
    points
        .into_iter()
        .filter_map(|mut p| {
            if !p.time_beat.is_finite() || p.time_beat < 0.0 {
                return None;
            }
            p.value_norm = if p.value_norm.is_finite() {
                p.value_norm.clamp(0.0, 1.0)
            } else {
                return None;
            };
            Some(p)
        })
        .collect()
}

/// `ClipContent` 内部の値域 sanitize。Midi notes / Audio events / Automation points を
/// 既存 sanitizer や finite チェックで掃除する。Video/Image/Text overlay content は別系統
/// (coverage invariant が load 時に再確立) なので触らない。
pub fn sanitize_content(content: &mut ClipContent) {
    match content {
        ClipContent::Midi(m) => {
            let notes = std::mem::take(&mut m.notes);
            m.notes = sanitize_notes(notes);
        }
        ClipContent::Audio(a) => {
            let events = std::mem::take(&mut a.events);
            a.events = sanitize_audio_events(events);
        }
        ClipContent::Automation(a) => {
            // automation point は plain value (target 依存で値域不明) なので clamp せず、
            // 非有限な time / value を持つ点だけ落とす。
            a.points
                .retain(|p| p.time_beat.is_finite() && p.time_beat >= 0.0 && p.value.is_finite());
        }
        _ => {}
    }
}

/// 外部 clipboard 由来の `ClipCopy` 群を sanitize。length_beats が非有限/<=0 のものは
/// 破棄、start_beat 非有限は 0、content 内部も sanitize する。
pub fn sanitize_clips(clips: Vec<ClipCopy>) -> Vec<ClipCopy> {
    clips
        .into_iter()
        .filter_map(|mut c| {
            if !c.length_beats.is_finite() || c.length_beats <= 0.0 {
                return None;
            }
            if !c.start_beat.is_finite() {
                c.start_beat = 0.0;
            }
            // 窓 offset は負も正当 (左端を外へ伸ばした clip) だが、非有限は先頭扱い。
            if !c.content_offset_beats.is_finite() {
                c.content_offset_beats = 0.0;
            }
            sanitize_content(&mut c.content);
            Some(c)
        })
        .collect()
}

/// 外部 clipboard 由来の `AutomationClipCopy` 群を sanitize。length_beats が非有限/<=0 の
/// ものは破棄、start_beat 非有限は 0、各 curve 点は `sanitize_points` (time 非有限/負を破棄 +
/// value_norm を 0..=1 clamp) で掃除する。
pub fn sanitize_automation_clips(clips: Vec<AutomationClipCopy>) -> Vec<AutomationClipCopy> {
    clips
        .into_iter()
        .filter_map(|mut c| {
            if !c.length_beats.is_finite() || c.length_beats <= 0.0 {
                return None;
            }
            if !c.start_beat.is_finite() {
                c.start_beat = 0.0;
            }
            c.points = sanitize_points(std::mem::take(&mut c.points));
            Some(c)
        })
        .collect()
}

/// 貼り付けの相対座標の上限 (行 / 列とも)。
const MAX_PASTE_OFFSET: i64 = 256;

/// r.md #87: 外部 clipboard 由来の `LauncherCellCopy` 群を sanitize。
/// 中身の検証は [`sanitize_clips`] / [`sanitize_automation_clips`] に委譲し
/// (規則を二重に持たない)、ここは **セル固有の 2 点**だけを見る:
/// 相対位置が現実的な範囲か、フォローアクションの値域が壊れていないか。
#[must_use]
pub fn sanitize_launcher_cells(cells: Vec<LauncherCellCopy>) -> Vec<LauncherCellCopy> {
    cells
        .into_iter()
        .filter_map(|mut c| {
            // 相対位置は選択群の広がりぶんしか出ない。桁違いの値は改竄なので捨てる
            // (`ensure_scene_at` に巨大 index を渡すと列を大量生成してしまう)。
            // 相対座標の上限。外部由来の JSON で数千列が生えるのを防ぐ
            // (現実の選択でこの値を超えることはない)。
            if !(0..MAX_PASTE_OFFSET).contains(&c.row_offset)
                || !(0..MAX_PASTE_OFFSET).contains(&c.scene_offset)
            {
                return None;
            }
            sanitize_launch(&mut c.launch);
            c.cell = match c.cell {
                LauncherCellPayload::Track(cc) => {
                    LauncherCellPayload::Track(sanitize_clips(vec![cc]).pop()?)
                }
                LauncherCellPayload::Lane(ac) => {
                    LauncherCellPayload::Lane(sanitize_automation_clips(vec![ac]).pop()?)
                }
            };
            Some(c)
        })
        .collect()
}

/// [`common::model::LaunchSettings`] の値域を正す。セル単体の貼り付け
/// ([`sanitize_launcher_cells`]) とトラックまるごとの貼り付け ([`sanitize_tracks`]) が
/// **同じ規則**を見るように 1 本にする (片方だけ直すと、同じ壊れた JSON でも
/// どちらの経路で入ったかで結果が変わる)。
pub fn sanitize_launch(launch: &mut common::model::LaunchSettings) {
    launch.follow.chance_a = launch.follow.chance_a.min(100);
    launch.follow.multiplier = launch.follow.multiplier.max(1);
    if !launch.follow.time_beats.is_finite() {
        launch.follow.time_beats = 4.0;
    }
    launch.follow.time_beats = launch.follow.time_beats.clamp(0.0625, 512.0);
}

/// クリップの窓 (`start_beat` / `content_offset_beats`) の非有限を正す。
/// 長さは「捨てる」判断が入れ物ごとに違うので呼び出し側の `retain` が見る。
fn sanitize_clip_window(start_beat: &mut f64, content_offset_beats: &mut f64) {
    if !start_beat.is_finite() {
        *start_beat = 0.0;
    }
    // 窓 offset は負も正当 (左端を外へ伸ばした clip) だが、非有限は先頭扱い。
    if !content_offset_beats.is_finite() {
        *content_offset_beats = 0.0;
    }
}

/// 外部 clipboard 由来の [`TracksCopy`] を sanitize。各 clip / automation lane clip /
/// **ランチャーのセル** の length_beats を検証 (不正は破棄)、窓を正し、volume/pan を
/// clamp、content payload を sanitize する。
pub fn sanitize_tracks(mut payload: TracksCopy) -> TracksCopy {
    // 列の並びは貼り先で `Song::ensure_scene_at` に渡す index になるので、
    // 長さに上限を掛けないと 1 回の貼り付けで列が大量に生える
    // (`sanitize_launcher_cells` の相対座標上限と同じ考え方)。
    payload.scenes.truncate(MAX_PASTE_OFFSET as usize);
    // 重複した列 id は「元で何列目だったか」を一意に解けなくする。index を詰めると
    // 他のセルの着地列までずれるので、**2 度目以降を未採番 sentinel (`0`) に潰す**
    // — そこを指すセルだけが貼り付け時に落ちる。
    let mut seen_scene: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for id in &mut payload.scenes {
        if *id == 0 || !seen_scene.insert(*id) {
            *id = 0;
        }
    }
    for tc in &mut payload.tracks {
        tc.track
            .clips
            .retain(|c| c.length_beats.is_finite() && c.length_beats > 0.0);
        for c in &mut tc.track.clips {
            sanitize_clip_window(&mut c.start_beat, &mut c.content_offset_beats);
        }
        // v35 (r.md #87): ランチャーのセルも同じ検査を通す。セルの長さは
        // **そのまま engine のループ長**になるので、NaN / 0 長を素通しすると
        // 「撃った瞬間に固まる行」を外部 JSON から作れてしまう。
        tc.track
            .session_clips
            .retain(|s| s.clip.length_beats.is_finite() && s.clip.length_beats > 0.0);
        for s in &mut tc.track.session_clips {
            sanitize_clip_window(&mut s.clip.start_beat, &mut s.clip.content_offset_beats);
            // セルは「撃った瞬間」が原点 (`SessionClip::clip` の契約)。
            s.clip.start_beat = 0.0;
            sanitize_launch(&mut s.launch);
        }
        for lane in &mut tc.track.automation_lanes {
            lane.clips
                .retain(|c| c.length_beats.is_finite() && c.length_beats > 0.0);
            lane.session_clips
                .retain(|s| s.clip.length_beats.is_finite() && s.clip.length_beats > 0.0);
            for s in &mut lane.session_clips {
                sanitize_clip_window(&mut s.clip.start_beat, &mut s.clip.content_offset_beats);
                s.clip.start_beat = 0.0;
                sanitize_launch(&mut s.launch);
            }
        }
        for ce in &mut tc.contents {
            sanitize_content(&mut ce.content);
        }
        tc.track.volume = if tc.track.volume.is_finite() {
            tc.track.volume.clamp(0.0, 2.0)
        } else {
            1.0
        };
        tc.track.pan = if tc.track.pan.is_finite() {
            tc.track.pan.clamp(-1.0, 1.0)
        } else {
            0.0
        };
    }
    payload
}

/// 外部 clipboard 由来の `DeviceCopy` 群を sanitize (`sanitize_tracks` と同じ流儀)。
/// `plugin_id` が空のものは捨て、id は必ず 0 に落とし (貼り先で新採番する)、
/// aux 参照の Vec 長だけ上限を掛ける。 aux の dangling track 参照は貼り付け時に
/// 実在判定で落とすので、 ここでは見ない。 `ports` は bool の集合で値域が無い。
pub fn sanitize_devices(devices: Vec<DeviceCopy>) -> Vec<DeviceCopy> {
    /// aux 入出力ポートの上限 (外部 clipboard が巨大 Vec を送り込むのを防ぐ)。
    const MAX_AUX_PORTS: usize = 64;
    devices
        .into_iter()
        .filter(|d| !d.device.plugin_id.is_empty())
        .map(|mut d| {
            d.device.id = 0;
            d.device.aux_inputs.truncate(MAX_AUX_PORTS);
            d.device.aux_outputs.truncate(MAX_AUX_PORTS);
            d
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(start: f64, dur: f64, pitch: u8, vel: u8) -> Note {
        Note {
            id: 0,
            start_beat: start,
            duration_beats: dur,
            pitch,
            velocity: vel,
            lyric: None,
            muted: false,
        }
    }

    #[test]
    fn envelope_roundtrip_notes() {
        let env = ClipboardEnvelope::new(
            42,
            ClipboardPayload::Notes(vec![note(0.0, 1.0, 60, 100)]),
        );
        let json = env.to_json().unwrap();
        let back = ClipboardEnvelope::from_json(&json).unwrap();
        assert_eq!(back.source_project_id, 42);
        match back.payload {
            ClipboardPayload::Notes(n) => {
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].pitch, 60);
            }
            _ => panic!("wrong payload variant"),
        }
    }

    /// r.md #71 (プラグインのコピー / 移動): device payload が JSON 往復する。
    /// `state` は base64 で載るので、 中身が保たれることも見る。
    #[test]
    fn envelope_roundtrip_devices() {
        use common::plugin_format::PluginFormat;
        let mut inst =
            common::model::PluginInstance::new("test.comp".into(), PluginFormat::Clap);
        inst.state = Some(std::sync::Arc::from(&b"knob"[..]));
        let env = ClipboardEnvelope::new(
            7,
            ClipboardPayload::Devices(vec![DeviceCopy {
                order: 0,
                source_track: 3,
                device: inst,
            }]),
        );
        let json = env.to_json().unwrap();
        let back = ClipboardEnvelope::from_json(&json).unwrap();
        assert_eq!(back.source_project_id, 7);
        match back.payload {
            ClipboardPayload::Devices(d) => {
                assert_eq!(d.len(), 1);
                assert_eq!(d[0].source_track, 3);
                assert_eq!(d[0].device.plugin_id, "test.comp");
                assert_eq!(d[0].device.state.as_deref(), Some(&b"knob"[..]));
            }
            _ => panic!("wrong payload variant"),
        }
    }

    #[test]
    fn sanitize_devices_drops_id_and_empty_id() {
        use common::plugin_format::PluginFormat;
        let dev = |plugin_id: &str, id: u64| DeviceCopy {
            order: 0,
            source_track: 1,
            device: common::model::PluginInstance {
                id,
                aux_inputs: vec![None; 200],
                aux_outputs: vec![None; 200],
                ..common::model::PluginInstance::new(plugin_id.into(), PluginFormat::Clap)
            },
        };
        let out = sanitize_devices(vec![dev("test.comp", 99), dev("", 1)]);
        assert_eq!(out.len(), 1, "plugin_id が空の device は捨てる");
        assert_eq!(out[0].device.id, 0, "id は貼り先で採番するので 0 に落とす");
        assert_eq!(out[0].device.aux_inputs.len(), 64);
        assert_eq!(out[0].device.aux_outputs.len(), 64);
    }

    #[test]
    fn from_json_rejects_foreign_and_garbage() {
        // 他アプリの素の text。
        assert!(ClipboardEnvelope::from_json("hello world").is_none());
        // magic 違いの JSON。
        let bad = r#"{"magic":"other.app","source_project_id":1,"payload":{"Notes":[]}}"#;
        assert!(ClipboardEnvelope::from_json(bad).is_none());
    }

    #[test]
    fn sanitize_notes_clamps_and_drops() {
        let input = vec![
            note(0.0, 1.0, 200, 200),          // pitch/vel over → clamp
            note(-1.0, 1.0, 60, 100),          // negative start → drop
            note(0.0, 0.0, 60, 100),           // zero duration → drop
            note(f64::NAN, 1.0, 60, 100),      // NaN start → drop
            note(2.0, 1.0, 60, 0),             // velocity 0 → clamp to 1
        ];
        let out = sanitize_notes(input);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].pitch, 127);
        assert_eq!(out[0].velocity, 127);
        assert_eq!(out[1].velocity, 1);
    }

    #[test]
    fn sanitize_clips_drops_bad_length_and_keeps_valid() {
        use common::model::ContentId;
        let mk = |len: f64, start: f64| ClipCopy {
            track_offset: 0,
            start_beat: start,
            length_beats: len,
            color: None,
            muted: false,
            content_id: 0 as ContentId,
            content_offset_beats: 0.0,
            content: ClipContent::default(),
            name: None,
            speaker_id: 0,
            singer_name: String::new(),
            style_name: String::new(),
            talk: None,
        };
        let out = sanitize_clips(vec![
            mk(4.0, 0.0),          // valid
            mk(f64::NAN, 0.0),     // NaN length → drop
            mk(0.0, 0.0),          // zero length → drop
            mk(-1.0, 0.0),         // negative length → drop
            mk(2.0, f64::NAN),     // NaN start → kept, start reset to 0
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].start_beat, 0.0);
    }

    #[test]
    fn sanitize_automation_clips_drops_bad_and_cleans_points() {
        use common::model::ContentId;
        let pt = |t: f64, v: f32| CopiedPoint {
            time_beat: t,
            value_norm: v,
            curve: AutomationCurve::Linear,
        };
        let mk = |len: f64, start: f64, pts: Vec<CopiedPoint>| AutomationClipCopy {
            start_beat: start,
            length_beats: len,
            source_content_id: 0 as ContentId,
            points: pts,
            name: None,
        };
        let out = sanitize_automation_clips(vec![
            mk(4.0, 0.0, vec![pt(0.0, 2.0), pt(-1.0, 0.5)]), // valid; point clamp + drop bad time
            mk(f64::NAN, 0.0, vec![]),                       // NaN length → drop
            mk(0.0, 0.0, vec![]),                            // zero length → drop
            mk(2.0, f64::NAN, vec![pt(0.0, 0.5)]),           // NaN start → kept, start reset to 0
        ]);
        assert_eq!(out.len(), 2);
        // first clip: out-of-range value_norm clamped to 1.0, negative-time point dropped.
        assert_eq!(out[0].points.len(), 1);
        assert_eq!(out[0].points[0].value_norm, 1.0);
        // NaN start reset to 0.
        assert_eq!(out[1].start_beat, 0.0);
    }

    #[test]
    fn envelope_roundtrip_automation_clips() {
        let env = ClipboardEnvelope::new(
            7,
            ClipboardPayload::AutomationClips(vec![AutomationClipCopy {
                start_beat: 1.5,
                length_beats: 4.0,
                source_content_id: 9,
                points: vec![CopiedPoint {
                    time_beat: 0.0,
                    value_norm: 0.25,
                    curve: AutomationCurve::Linear,
                }],
                name: Some("Volume curve".to_string()),
            }]),
        );
        let json = env.to_json().unwrap();
        let back = ClipboardEnvelope::from_json(&json).unwrap();
        match back.payload {
            ClipboardPayload::AutomationClips(cs) => {
                assert_eq!(cs.len(), 1);
                assert_eq!(cs[0].length_beats, 4.0);
                assert_eq!(cs[0].source_content_id, 9);
                assert_eq!(cs[0].points.len(), 1);
                assert_eq!(cs[0].name.as_deref(), Some("Volume curve"));
            }
            _ => panic!("wrong payload variant"),
        }
    }

    #[test]
    fn sanitize_points_clamps_norm_and_drops_bad_time() {
        let input = vec![
            CopiedPoint { time_beat: 0.0, value_norm: 2.0, curve: AutomationCurve::Linear },
            CopiedPoint { time_beat: -1.0, value_norm: 0.5, curve: AutomationCurve::Linear },
            CopiedPoint { time_beat: f64::INFINITY, value_norm: 0.5, curve: AutomationCurve::Linear },
        ];
        let out = sanitize_points(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value_norm, 1.0);
    }
}
