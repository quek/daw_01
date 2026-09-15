//! 取り込み / 生成したメディアの **置き場と Song への記録形** の SSoT。
//!
//! 保存済みプロジェクトは bundle (`<project_dir>/{samples,images,bounce}/`) に置いて
//! `ProjectRelative` で記録し、未保存プロジェクトは per-user のキャッシュに置いて `Absolute` で
//! 記録する (保存時に bundle へ移る: `import_audio::plan_unsaved_*_migration`)。
//!
//! 以前はこの分岐が経路ごと (音声 / 動画 / 画像 / Global Sampler / Bounce) に手写しされ、未保存側は
//! `AppDirs::production()` を直接引いていた。`AppData` に注入された `AppDirs` を素通りするので、
//! `app_dirs = None` のテストまで `%LOCALAPPDATA%\daw_01\import_cache` へ書き、同じ内容の動画を
//! 取り込む並行テスト同士が同じ WAV を奪い合っていた (ユーザーの起動中の daw_gui とも共有)。
//! 未保存側の置き場は **注入された `AppDirs` からだけ** 解決する。

use std::path::{Path, PathBuf};

use common::app_dirs::AppDirs;
use common::model::{AudioSourcePath, ImageSourcePath, VideoSourcePath};

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

impl MediaPool {
    /// 保存済みプロジェクトでのサブフォルダ名。
    pub fn bundle_subdir(self) -> &'static str {
        match self {
            Self::Samples => "samples",
            Self::Images => "images",
            Self::Bounce => "bounce",
        }
    }

    /// 未保存プロジェクトでの置き場。
    pub fn unsaved_dir(self, dirs: &AppDirs) -> PathBuf {
        match self {
            Self::Samples | Self::Images => dirs.import_cache_dir(),
            Self::Bounce => dirs.bounce_cache_dir(),
        }
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
    /// `project_dir` があれば bundle、無ければ `app_dirs` のキャッシュ。どちらも無ければ `None`。
    pub fn resolve(
        pool: MediaPool,
        project_dir: Option<&Path>,
        app_dirs: Option<&AppDirs>,
    ) -> Option<Self> {
        match (project_dir, app_dirs) {
            (Some(dir), _) => Some(Self::bundle(dir, pool)),
            (None, Some(dirs)) => Some(Self::unsaved(pool.unsaved_dir(dirs))),
            (None, None) => None,
        }
    }

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
