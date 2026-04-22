use crate::protocol::ChildKind;

pub fn pipe_path(parent_pid: u32, kind: ChildKind) -> String {
    format!(r"\\.\pipe\daw_01_{}_{}", parent_pid, kind.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_pipe_path() {
        assert_eq!(
            pipe_path(1234, ChildKind::Audio),
            r"\\.\pipe\daw_01_1234_audio"
        );
    }

    #[test]
    fn plugin_host_pipe_path() {
        assert_eq!(
            pipe_path(5678, ChildKind::PluginHost),
            r"\\.\pipe\daw_01_5678_plugin_host"
        );
    }
}
