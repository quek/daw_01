//! コードトラック (r.md #157、`docs/plan_chord_track.md`) の型。
//!
//! `Song.chords` / `Song.audio_chords` / `Track.follow_chords` として `LoadSong` の wire を
//! 渡るので `common/build.rs` の `WIRE_SOURCES` に登録している (不変条件 7)。帯の編集ロジックは
//! `chord_ops.rs`、追従の評価は `crate::chord_follow`、名前の読み書きは `crate::chord_name` に置く
//! (どれも wire に載らないので、ロジックの変更で fingerprint を動かさない)。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// コードの種類 (3 和音 + 6th / 7th まで)。テンションと変化音は [`Tensions`] に分ける。
///
/// 並びは UI のボタン順の SSoT ([`ChordKind::ALL`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode)]
pub enum ChordKind {
    #[default]
    Major,
    Minor,
    Diminished,
    Augmented,
    Sus2,
    Sus4,
    /// ルートと 5 度だけ (`C5`)。
    Power,
    Major6,
    Minor6,
    Dominant7,
    Major7,
    Minor7,
    MinorMajor7,
    Diminished7,
    /// `m7b5`。
    HalfDiminished7,
    Augmented7,
    AugmentedMajor7,
    Dominant7Sus4,
}

impl ChordKind {
    /// UI のボタン順 (全種類)。
    pub const ALL: &'static [ChordKind] = &[
        ChordKind::Major,
        ChordKind::Minor,
        ChordKind::Diminished,
        ChordKind::Augmented,
        ChordKind::Sus2,
        ChordKind::Sus4,
        ChordKind::Power,
        ChordKind::Major6,
        ChordKind::Minor6,
        ChordKind::Dominant7,
        ChordKind::Major7,
        ChordKind::Minor7,
        ChordKind::MinorMajor7,
        ChordKind::Diminished7,
        ChordKind::HalfDiminished7,
        ChordKind::Augmented7,
        ChordKind::AugmentedMajor7,
        ChordKind::Dominant7Sus4,
    ];

    /// ルートからの半音 (昇順、ルートの 0 を含む)。
    #[must_use]
    pub const fn intervals(self) -> &'static [u8] {
        match self {
            ChordKind::Major => &[0, 4, 7],
            ChordKind::Minor => &[0, 3, 7],
            ChordKind::Diminished => &[0, 3, 6],
            ChordKind::Augmented => &[0, 4, 8],
            ChordKind::Sus2 => &[0, 2, 7],
            ChordKind::Sus4 => &[0, 5, 7],
            ChordKind::Power => &[0, 7],
            ChordKind::Major6 => &[0, 4, 7, 9],
            ChordKind::Minor6 => &[0, 3, 7, 9],
            ChordKind::Dominant7 => &[0, 4, 7, 10],
            ChordKind::Major7 => &[0, 4, 7, 11],
            ChordKind::Minor7 => &[0, 3, 7, 10],
            ChordKind::MinorMajor7 => &[0, 3, 7, 11],
            ChordKind::Diminished7 => &[0, 3, 6, 9],
            ChordKind::HalfDiminished7 => &[0, 3, 6, 10],
            ChordKind::Augmented7 => &[0, 4, 8, 10],
            ChordKind::AugmentedMajor7 => &[0, 4, 8, 11],
            ChordKind::Dominant7Sus4 => &[0, 5, 7, 10],
        }
    }

    /// 短 3 度を持つ種類か (度数表記の大文字 / 小文字、既定スケールの選び分けに使う)。
    #[must_use]
    pub const fn is_minor(self) -> bool {
        matches!(
            self,
            ChordKind::Minor
                | ChordKind::Diminished
                | ChordKind::Minor6
                | ChordKind::Minor7
                | ChordKind::MinorMajor7
                | ChordKind::Diminished7
                | ChordKind::HalfDiminished7
        )
    }
}

/// テンションと変化音の集合 (bit ごとに 1 音)。
///
/// `FLAT5` / `SHARP5` は **5 度を置き換える** (`C7b5` は 5 度を持たない)。それ以外は足す音。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode)]
#[serde(transparent)]
pub struct Tensions(pub u16);

impl Tensions {
    pub const NONE: Tensions = Tensions(0);
    pub const FLAT9: Tensions = Tensions(1 << 0);
    pub const NINE: Tensions = Tensions(1 << 1);
    pub const SHARP9: Tensions = Tensions(1 << 2);
    pub const ELEVEN: Tensions = Tensions(1 << 3);
    pub const SHARP11: Tensions = Tensions(1 << 4);
    pub const FLAT13: Tensions = Tensions(1 << 5);
    pub const THIRTEEN: Tensions = Tensions(1 << 6);
    pub const FLAT5: Tensions = Tensions(1 << 7);
    pub const SHARP5: Tensions = Tensions(1 << 8);

    /// UI のボタン順 (全種類) と表示名、ルートからの半音 (オクターブ内)。
    pub const ALL: &'static [(Tensions, &'static str, u8)] = &[
        (Tensions::FLAT9, "b9", 1),
        (Tensions::NINE, "9", 2),
        (Tensions::SHARP9, "#9", 3),
        (Tensions::ELEVEN, "11", 5),
        (Tensions::SHARP11, "#11", 6),
        (Tensions::FLAT13, "b13", 8),
        (Tensions::THIRTEEN, "13", 9),
        (Tensions::FLAT5, "b5", 6),
        (Tensions::SHARP5, "#5", 8),
    ];

    const VALID_BITS: u16 = (1 << 9) - 1;

    #[must_use]
    pub const fn contains(self, t: Tensions) -> bool {
        self.0 & t.0 == t.0 && t.0 != 0
    }

    #[must_use]
    pub const fn with(self, t: Tensions) -> Tensions {
        Tensions(self.0 | t.0)
    }

    #[must_use]
    pub const fn without(self, t: Tensions) -> Tensions {
        Tensions(self.0 & !t.0)
    }

    #[must_use]
    pub const fn toggled(self, t: Tensions) -> Tensions {
        Tensions(self.0 ^ t.0)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// 未定義の bit を落とす (disk / clipboard から来た値)。
    #[must_use]
    pub const fn sanitized(self) -> Tensions {
        Tensions(self.0 & Self::VALID_BITS)
    }
}

/// 音名の綴り (`C#` か `Db` か)。`Auto` はその位置のキー ([`crate::scale::prefers_flats`]) に従う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode)]
pub enum Spelling {
    #[default]
    Auto,
    Sharp,
    Flat,
}

/// コード 1 つ (ルート + 種類 + テンション + ベース)。
///
/// 音程は **書いた音の空間** — グローバルトランスポーズ (r.md #130) は鳴らす段で足す
/// (Key / スケールと同じ扱い)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode)]
pub struct Chord {
    /// ルートの pitch class (`0..=11`、0 = C)。
    pub root: u8,
    #[serde(default)]
    pub kind: ChordKind,
    #[serde(default, skip_serializing_if = "Tensions::is_empty_ref")]
    pub tensions: Tensions,
    /// ベース音の pitch class (`C/E` の E)。`None` = ルート。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bass: Option<u8>,
    #[serde(default, skip_serializing_if = "Spelling::is_auto")]
    pub spelling: Spelling,
}

impl Tensions {
    fn is_empty_ref(&self) -> bool {
        self.is_empty()
    }
}

impl Spelling {
    fn is_auto(&self) -> bool {
        *self == Spelling::Auto
    }
}

impl Chord {
    /// ルートと種類だけのコード。
    #[must_use]
    pub const fn new(root: u8, kind: ChordKind) -> Chord {
        Chord { root: root % 12, kind, tensions: Tensions::NONE, bass: None, spelling: Spelling::Auto }
    }

    /// ルートからの半音の集合 (bit 0 = ルート、12-bit)。テンションはオクターブ内へ畳む。
    /// ベース音は**含めない** (ベースは別の声部。[`Chord::bass_pc`])。
    #[must_use]
    pub fn interval_mask(&self) -> u16 {
        let mut mask = 0u16;
        for &i in self.kind.intervals() {
            mask |= 1 << i;
        }
        let altered_fifth =
            self.tensions.contains(Tensions::FLAT5) || self.tensions.contains(Tensions::SHARP5);
        if altered_fifth {
            mask &= !(1 << 7);
        }
        for &(t, _, semis) in Tensions::ALL {
            if self.tensions.contains(t) {
                mask |= 1 << semis;
            }
        }
        mask
    }

    /// 構成音の pitch class の集合 (bit 0 = C、12-bit)。ベース音は含めない。
    #[must_use]
    pub fn pitch_class_mask(&self) -> u16 {
        let m = self.interval_mask();
        let r = u32::from(self.root % 12);
        // 12-bit の回転 (ルートぶん上へ)。
        (((u32::from(m) << r) | (u32::from(m) >> (12 - r))) & 0x0FFF) as u16
    }

    /// `pitch` (MIDI 鍵盤) の pitch class が構成音か。
    #[must_use]
    pub fn contains(&self, pitch: u8) -> bool {
        (self.pitch_class_mask() >> (pitch % 12)) & 1 == 1
    }

    /// ベース音の pitch class (指定が無ければルート)。
    #[must_use]
    pub fn bass_pc(&self) -> u8 {
        self.bass.unwrap_or(self.root) % 12
    }

    /// 値域を揃える (disk / clipboard / script から来た値)。
    #[must_use]
    pub fn sanitized(mut self) -> Chord {
        self.root %= 12;
        self.bass = self.bass.map(|b| b % 12).filter(|b| *b != self.root);
        self.tensions = self.tensions.sanitized();
        self
    }

    /// 全体を `semitones` 移調したコード (ルートとベース)。
    #[must_use]
    pub fn transposed(mut self, semitones: i32) -> Chord {
        let shift = |pc: u8| (i32::from(pc) + semitones).rem_euclid(12) as u8;
        self.root = shift(self.root % 12);
        self.bass = self.bass.map(|b| shift(b % 12));
        self
    }
}

/// コードトラック上のコード 1 個 (長さを持つ箱、Studio One 型)。`Song.chords` に
/// `start_beat` 昇順・互いに非交差 (隙間は許容) で持つ ([`crate::model::Song::normalize_chords`])。
/// 箱の無い区間ではどのトラックも追従しない (書いた音のまま)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ChordEvent {
    /// Song 内で安定な id (`Song::alloc_chord_id`、`0` は未採番 sentinel)。
    pub id: u32,
    /// 開始拍 (song-absolute)。
    pub start_beat: f64,
    /// 長さ (拍)。
    pub len_beats: f64,
    pub chord: Chord,
    /// 箱の塗り色 (RGB、`0.0..=1.0`)。`None` = テーマの既定色。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
}

impl ChordEvent {
    /// 終端拍 (= `start_beat + len_beats`)。
    #[must_use]
    pub fn end_beat(&self) -> f64 {
        self.start_beat + self.len_beats
    }
}

/// 素材 (クリップの中身) が **もともと鳴らしているコード** の 1 区間。位置は拍 (何の拍かは使う側が決める:
/// MIDI の検出結果は content-local、コードトラックへ取り込むときは song-absolute)。
///
/// 平行移動 / ベースの追従は「素材のコード → コードトラックのコード」の差で動かすので、
/// 素材側のコードが要る。MIDI はノートから毎回導く (`crate::chord_detect`) ので保存しない。
/// オーディオは検出が重いので、音源ごとの結果を [`AudioChordSpan`] で `Song.audio_chords` に持つ。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct SourceChord {
    pub start_beat: f64,
    pub len_beats: f64,
    pub chord: Chord,
}

/// オーディオ音源から検出したコードの 1 区間。位置は **音源のフレーム** (音源ファイルだけで決まるので、
/// クリップの trim / 分割 / 伸縮 / テンポ変更で狂わない)。`Song.audio_chords[source_id]` に
/// `start_frame` 昇順・非交差で持つ。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioChordSpan {
    pub start_frame: u64,
    pub end_frame: u64,
    pub chord: Chord,
    /// r.md #159: この区間で **同時に 3 つ以上の音が鳴っている** (和音を弾いている) か。`true` の区間だけが
    /// 自分の和音として追従の基準になる (`crate::chord_detect::audio_own_chord_at`)。歌声や単音の楽器を窓で
    /// 集めた結果 (旋律の音が偶然コードに見えたもの) は `false` で、帯へ抽出するときにだけ使う。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub chordal: bool,
}

/// クリップ 1 つの **書かれたときのコード** の記録 (r.md #159、`Clip::chord_basis`)。
///
/// 和音を持たない素材 (メロディー / ベースライン / 歌声) の追従の基準。コードの箱を **書き換えた** とき
/// (`Song::set_chord` / `Song::transpose_chords`) にだけ、その箱の下にあって記録の無い素材へ、書き換える前の
/// コードを書く。記録の無い素材は今のコードが基準 = 動かない (置いただけでは動かない)。規則の正本は
/// `crate::model::chord_basis` の module doc。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ClipChordBasis {
    /// note id (clip の content の `Note::id`) 昇順・重複なし。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<NoteChordBasis>,
    /// `(take, start_frame)` 昇順。同じ take の中は互いに重ならない。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio: Vec<AudioChordBasis>,
}

impl ClipChordBasis {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty() && self.audio.is_empty()
    }
}

/// ノート 1 つが書かれたときのコード。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct NoteChordBasis {
    pub note_id: u32,
    pub chord: Chord,
}

/// オーディオの take (`AudioEvent::take_key`) の音源フレーム `[start_frame, end_frame)` が書かれたときのコード。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioChordBasis {
    pub take: u32,
    pub start_frame: u64,
    pub end_frame: u64,
    pub chord: Chord,
}

/// コードを鍵盤にする並べ方 (Cubase の Voicings: Piano / Guitar / Basic + Adaptive Voicings)。
/// 鍵盤の決め方は `crate::chord_voicing::voice_chord`。試聴の設定 (`AudioCommand::SetChordAudition`)
/// として wire を渡るのでここに置く (Song には載らない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode)]
pub enum Voicing {
    /// 基本形: ベース音を 1 オクターブ下に 1 つ、その上にルートから積む (Studio One のノート化と同じ形)。
    #[default]
    Basic,
    /// ピアノ: 左手にベース音 1 つ、右手に構成音を密集位置で (`base` の 5 度上から) 積む。
    Piano,
    /// ギター: 標準調弦 (E A D G B E) の 6 弦で、ローポジションから押さえられる形を選ぶ。
    Guitar,
    /// 自動: 直前のコードからの声部の動きが最小になる転回形を選ぶ (Cubase の Adaptive Voicings)。
    /// 直前が無ければピアノと同じ。
    Adaptive,
}

impl Voicing {
    /// 見出し側の選択肢の並び (全種類) と表示名。
    pub const ALL: &'static [(Voicing, &'static str)] = &[
        (Voicing::Basic, "基本形"),
        (Voicing::Piano, "ピアノ"),
        (Voicing::Guitar, "ギター"),
        (Voicing::Adaptive, "自動 (声部を滑らかに)"),
    ];
}

/// トラックがコードトラックにどう追従するか (r.md #157 Q2 / Q6)。既定は追従しない。
///
/// 非破壊 — ノートのデータは書き換えず、鳴らす音だけが変わる。評価は
/// [`crate::chord_follow`] 1 本。オーディオクリップは全体を平行移動するだけなので、
/// `Narrow` / `Scale` も `Parallel` と同じに扱う (和音の中の 1 音だけは動かせない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode)]
pub enum ChordFollow {
    #[default]
    Off,
    /// 素材のコードのルートを今のコードのルートへ合わせて全体を平行移動する。
    Parallel,
    /// 各ノートを今のコードの構成音のうち最寄りへ寄せる。
    Narrow,
    /// 素材のコードのルートを今のコードのベース音へ合わせて平行移動する (ベーストラック向き)。
    Bass,
    /// 各ノートを今のコードに合うスケールの最寄りの音へ寄せる。
    Scale,
}

impl ChordFollow {
    /// メニューの並び順 (全種類)。
    pub const ALL: &'static [ChordFollow] = &[
        ChordFollow::Off,
        ChordFollow::Parallel,
        ChordFollow::Narrow,
        ChordFollow::Bass,
        ChordFollow::Scale,
    ];

    #[must_use]
    pub const fn is_off(&self) -> bool {
        matches!(self, ChordFollow::Off)
    }

    /// メニューの表示名。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            ChordFollow::Off => "Off",
            ChordFollow::Parallel => "平行移動",
            ChordFollow::Narrow => "最寄りのコード音",
            ChordFollow::Bass => "ベース",
            ChordFollow::Scale => "スケール",
        }
    }

    /// トラックヘッダの印 (1 文字)。`Off` は印なし。
    #[must_use]
    pub const fn mark(self) -> Option<&'static str> {
        match self {
            ChordFollow::Off => None,
            ChordFollow::Parallel => Some("P"),
            ChordFollow::Narrow => Some("N"),
            ChordFollow::Bass => Some("B"),
            ChordFollow::Scale => Some("S"),
        }
    }
}
