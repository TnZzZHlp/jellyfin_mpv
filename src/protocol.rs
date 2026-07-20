use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum RemoteCommand {
    Play(PlayRequest),
    Pause,
    Resume,
    TogglePause,
    Stop,
    Seek(i64),
    SetVolume(f64),
    Mute(bool),
    ToggleMute,
    SetSubtitle(Option<i32>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlayRequest {
    #[serde(default)]
    pub item_ids: Vec<String>,
    #[serde(default)]
    pub start_position_ticks: Option<i64>,
    #[serde(default)]
    pub media_source_id: Option<String>,
    #[serde(default)]
    pub audio_stream_index: Option<i32>,
    #[serde(default)]
    pub subtitle_stream_index: Option<i32>,
    #[serde(default)]
    pub start_index: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SocketEnvelope {
    message_type: String,
    #[serde(default)]
    data: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PlaystateRequest {
    command: String,
    #[serde(default)]
    seek_position_ticks: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GeneralCommand {
    name: String,
    #[serde(default)]
    arguments: HashMap<String, String>,
}

pub enum SocketAction {
    Command(RemoteCommand),
    ForceKeepAlive(u64),
    Ignore,
}

pub fn parse_socket_message(text: &str) -> Result<SocketAction, serde_json::Error> {
    let envelope: SocketEnvelope = serde_json::from_str(text)?;
    match envelope.message_type.as_str() {
        "ForceKeepAlive" => Ok(SocketAction::ForceKeepAlive(
            envelope.data.as_u64().unwrap_or(60).max(2),
        )),
        "Play" => {
            let request: PlayRequest = serde_json::from_value(envelope.data)?;
            Ok(SocketAction::Command(RemoteCommand::Play(request)))
        }
        "Playstate" => {
            let request: PlaystateRequest = serde_json::from_value(envelope.data)?;
            let command = match request.command.as_str() {
                "Pause" => Some(RemoteCommand::Pause),
                "Unpause" => Some(RemoteCommand::Resume),
                "PlayPause" => Some(RemoteCommand::TogglePause),
                "Stop" => Some(RemoteCommand::Stop),
                "Seek" => request.seek_position_ticks.map(RemoteCommand::Seek),
                _ => None,
            };
            Ok(command.map_or(SocketAction::Ignore, SocketAction::Command))
        }
        "GeneralCommand" => {
            let request: GeneralCommand = serde_json::from_value(envelope.data)?;
            let command = match request.name.as_str() {
                "SetVolume" => request
                    .arguments
                    .get("Volume")
                    .or_else(|| request.arguments.get("volume"))
                    .and_then(|value| value.parse::<f64>().ok())
                    .map(RemoteCommand::SetVolume),
                "Mute" => Some(RemoteCommand::Mute(true)),
                "Unmute" => Some(RemoteCommand::Mute(false)),
                "ToggleMute" => Some(RemoteCommand::ToggleMute),
                "SetSubtitleStreamIndex" => request
                    .arguments
                    .get("Index")
                    .or_else(|| request.arguments.get("SubtitleStreamIndex"))
                    .or_else(|| request.arguments.get("index"))
                    .and_then(|value| value.parse::<i32>().ok())
                    .map(|index| RemoteCommand::SetSubtitle((index >= 0).then_some(index))),
                _ => None,
            };
            Ok(command.map_or(SocketAction::Ignore, SocketAction::Command))
        }
        _ => Ok(SocketAction::Ignore),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserDto {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthenticationResult {
    pub access_token: String,
    pub user: UserDto,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ItemDto {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub media_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackInfoResponse {
    #[serde(default)]
    pub media_sources: Vec<MediaSource>,
    pub play_session_id: Option<String>,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSource {
    pub id: Option<String>,
    #[serde(default)]
    pub supports_direct_play: bool,
    pub transcoding_url: Option<String>,
    pub live_stream_id: Option<String>,
    pub default_audio_stream_index: Option<i32>,
    pub default_subtitle_stream_index: Option<i32>,
    #[serde(default)]
    pub media_streams: Vec<MediaStream>,
    #[serde(default)]
    pub media_attachments: Vec<MediaAttachment>,
    #[serde(default)]
    pub required_http_headers: HashMap<String, Option<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaAttachment {
    pub index: i32,
    pub codec: Option<String>,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub delivery_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaStream {
    pub index: i32,
    #[serde(rename = "Type")]
    pub stream_type: String,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub display_title: Option<String>,
    #[serde(default)]
    pub is_text_subtitle_stream: bool,
    pub delivery_method: Option<String>,
    pub delivery_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackReport<'a> {
    pub item_id: &'a str,
    pub media_source_id: &'a str,
    pub play_session_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_stream_id: Option<&'a str>,
    pub position_ticks: i64,
    pub can_seek: bool,
    pub is_paused: bool,
    pub is_muted: bool,
    pub volume_level: i32,
    pub play_method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackStopReport<'a> {
    pub item_id: &'a str,
    pub media_source_id: &'a str,
    pub play_session_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_stream_id: Option<&'a str>,
    pub position_ticks: i64,
    pub failed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_play_and_seek() {
        let play = r#"{"MessageType":"Play","Data":{"ItemIds":["abc"],"StartPositionTicks":42}}"#;
        let SocketAction::Command(RemoteCommand::Play(request)) =
            parse_socket_message(play).unwrap()
        else {
            panic!("不是播放命令");
        };
        assert_eq!(request.item_ids, ["abc"]);
        assert_eq!(request.start_position_ticks, Some(42));

        let seek =
            r#"{"MessageType":"Playstate","Data":{"Command":"Seek","SeekPositionTicks":99}}"#;
        assert!(matches!(
            parse_socket_message(seek).unwrap(),
            SocketAction::Command(RemoteCommand::Seek(99))
        ));
    }

    #[test]
    fn parses_pause_resume_and_toggle() {
        for (command, expected) in [
            ("Pause", RemoteCommand::Pause),
            ("Unpause", RemoteCommand::Resume),
            ("PlayPause", RemoteCommand::TogglePause),
        ] {
            let message =
                format!(r#"{{"MessageType":"Playstate","Data":{{"Command":"{command}"}}}}"#);
            let SocketAction::Command(actual) = parse_socket_message(&message).unwrap() else {
                panic!("未解析为播放状态命令");
            };
            assert_eq!(
                std::mem::discriminant(&actual),
                std::mem::discriminant(&expected)
            );
        }
    }

    #[test]
    fn parses_volume_and_mute() {
        let volume = r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetVolume","Arguments":{"Volume":"37"}}}"#;
        assert!(matches!(
            parse_socket_message(volume).unwrap(),
            SocketAction::Command(RemoteCommand::SetVolume(37.0))
        ));

        let mute = r#"{"MessageType":"GeneralCommand","Data":{"Name":"Mute","Arguments":{}}}"#;
        assert!(matches!(
            parse_socket_message(mute).unwrap(),
            SocketAction::Command(RemoteCommand::Mute(true))
        ));
    }

    #[test]
    fn parses_keepalive_timeout() {
        let keepalive = r#"{"MessageType":"ForceKeepAlive","Data":60}"#;
        assert!(matches!(
            parse_socket_message(keepalive).unwrap(),
            SocketAction::ForceKeepAlive(60)
        ));
    }

    #[test]
    fn parses_subtitle_selection_and_disable() {
        let select = r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetSubtitleStreamIndex","Arguments":{"Index":"4"}}}"#;
        assert!(matches!(
            parse_socket_message(select).unwrap(),
            SocketAction::Command(RemoteCommand::SetSubtitle(Some(4)))
        ));

        let disable = r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetSubtitleStreamIndex","Arguments":{"Index":"-1"}}}"#;
        assert!(matches!(
            parse_socket_message(disable).unwrap(),
            SocketAction::Command(RemoteCommand::SetSubtitle(None))
        ));
    }
}
