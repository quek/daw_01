//! 取り込み / 生成したメディアの **置き場と Song への記録形** の SSoT。
//!
//! 保存済みプロジェクトは bundle (`<project_dir>/{samples,images,bounce}/`) に置いて
//! `ProjectRelative` で記録し、未保存プロジェクトは per-user のキャッシュの **その文書の置き場**
//! (`<pool の親>/<DocId>/`) に置いて `Absolute` で記録する (保存時に bundle へ移る:
//! [`crate::media_bundle::plan_cache_transfer`])。
//!
//! 以前はこの分岐が経路ごと (音声 / 動画 / 画像 / Global Sampler / Bounce) に手写しされ、未保存側は
//! `AppDirs::production()` を直接引いていた。`AppData` に注入された `AppDirs` を素通りするので、
//! `app_dirs = None` のテストまで `%LOCALAPPDATA%\daw_01\import_cache` へ書き、同じ内容の動画を
//! 取り込む並行テスト同士が同じ WAV を奪い合っていた (ユーザーの起動中の daw_gui とも共有)。
//! 未保存側の置き場は **注入された `AppDirs` からだけ** 解決する。
//!
//! 置き場を文書ごとに分けるのは、内容で名前が決まるファイルを全文書で共有すると、片方の保存が
//! そのファイルを自分の bundle へ **move** し、同じ素材を取り込んでいたもう片方の絶対パスが
//! 消えるから。どの文書の置き場かはパスが持つ ([`MediaPool::transfer_mode`])。

use std::path::{Path, PathBuf};

use common::app_dirs::AppDirs;
use common::model::{AudioSourcePath, ImageSourcePath, VideoSourcePath};
use common::recovery::DocId;

/// 未保存で per-user データフォルダも無いときに status へ出す文言。
pub const NO_UNSAVED_DIR: &str =
    "未保存プロジェクトの素材置き場 (per-user データフォルダ) がありません。先に保存してください";

/// メディアのプール。bundle のサブフォルダと、未保存時のキャッシュを決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPool {
    /// 取り込んだ音声 / 動画 (と動画から抜いた音声)、Global Sampler の切り出し。
    Samples,
    /// 取り込んだ画像。
    Images,
    /// Bounce / Glue の出力。
    Bounce,
}

/// 未保存の置き場にあるファイルを、保存 (や別の文書への取り込み) で自分の置き場へ運ぶ方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferMode {
    /// 自分の置き場のもの。持ち主は自分だけなので移す。
    Move,
    /// 別の文書の置き場 (や文書ごとに分ける前の共有の置き場) のもの。持ち主がまだ使うので複製する。
    Copy,
}

impl MediaPool {
    /// 未保存の置き場の親を 1 回ずつ回すためのプール (`Images` の親は `Samples` と同じ)。
    pub const UNSAVED_ROOTS: [MediaPool; 2] = [MediaPool::Samples, MediaPool::Bounce];

    /// 保存済みプロジェクトでのサブフォルダ名。
    pub fn bundle_subdir(self) -> &'static str {
        match self {
            Self::Samples => "samples",
            Self::Images => "images",
            Self::Bounce => "bounce",
        }
    }

    /// 未保存の文書の置き場を並べる親 (`import_cache` / `bounce_cache`)。
    pub fn unsaved_root(self, dirs: &AppDirs) -> PathBuf {
        match self {
            Self::Samples | Self::Images => dirs.import_cache_dir(),
            Self::Bounce => dirs.bounce_cache_dir(),
        }
    }

    /// 未保存の文書 `doc` の置き場。
    pub fn unsaved_dir(self, dirs: &AppDirs, doc: DocId) -> PathBuf {
        self.unsaved_root(dirs).join(doc.to_string())
    }

    /// `abs` がこのプールの未保存の置き場にあるなら、文書 `own` へ運ぶ方法。置き場の外
    /// (bundle / 外部ファイル) なら `None`。
    pub fn transfer_mode(self, abs: &Path, dirs: &AppDirs, own: DocId) -> Option<TransferMode> {
        if abs.starts_with(self.unsaved_dir(dirs, own)) {
            Some(TransferMode::Move)
        } else if abs.starts_with(self.unsaved_root(dirs)) {
            Some(TransferMode::Copy)
        } else {
            None
        }
    }

    /// 音声ファイル `abs` が未保存の置き場にあるなら、そのプール (Bounce の出力か取り込みか)。
    pub fn of_unsaved_audio(abs: &Path, dirs: &AppDirs) -> Option<Self> {
        [Self::Bounce, Self::Samples].into_iter().find(|p| abs.starts_with(p.unsaved_root(dirs)))
    }
}

/// 1 つのプールの置き場。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaDest {
    /// 保存済みプロジェクトの bundle。`ProjectRelative(<subdir>/<file>)` で記録する。
    Bundle { project_dir: PathBuf, subdir: &'static str },
    /// 未保存プロジェクトのキャッシュ。`Absolute` で記録する。
    Unsaved { dir: PathBuf },
}

impl MediaDest {
    pub fn bundle(project_dir: &Path, pool: MediaPool) -> Self {
        Self::Bundle { project_dir: project_dir.to_path_buf(), subdir: pool.bundle_subdir() }
    }

    pub fn unsaved(dir: impl Into<PathBuf>) -> Self {
        Self::Unsaved { dir: dir.into() }
    }

    /// ファイルを書くフォルダ (絶対)。作るのは書き手。
    pub fn dir(&self) -> PathBuf {
        match self {
            Self::Bundle { project_dir, subdir } => project_dir.join(subdir),
            Self::Unsaved { dir } => dir.clone(),
        }
    }

    /// Song に記録する bundle 相対パス。未保存なら `None` (= 絶対パスで記録する)。
    fn relative(&self, filename: &str) -> Option<PathBuf> {
        match self {
            Self::Bundle { subdir, .. } => Some(Path::new(subdir).join(filename)),
            Self::Unsaved { .. } => None,
        }
    }

    pub fn audio_path(&self, filename: &str) -> AudioSourcePath {
        self.relative(filename).map_or_else(
            || AudioSourcePath::Absolute(self.dir().join(filename)),
            AudioSourcePath::ProjectRelative,
        )
    }

    pub fn video_path(&self, filename: &str) -> VideoSourcePath {
        self.relative(filename).map_or_else(
            || VideoSourcePath::Absolute(self.dir().join(filename)),
            VideoSourcePath::ProjectRelative,
        )
    }

    pub fn image_path(&self, filename: &str) -> ImageSourcePath {
        self.relative(filename).map_or_else(
            || ImageSourcePath::Absolute(self.dir().join(filename)),
            ImageSourcePath::ProjectRelative,
        )
    }
}
