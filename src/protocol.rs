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
    VolumeUp,
    VolumeDown,
    ToggleFullscreen,
    PlaybackRate(f64),
    Mute(bool),
    ToggleMute,
    SetAudio(Option<i32>),
    SetSubtitle(Option<i32>),
}

impl RemoteCommand {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Play(_) => "Play",
            Self::Pause => "Pause",
            Self::Resume => "Resume",
            Self::TogglePause => "TogglePause",
            Self::Stop => "Stop",
            Self::Seek(_) => "Seek",
            Self::SetVolume(_) => "SetVolume",
            Self::VolumeUp => "VolumeUp",
            Self::VolumeDown => "VolumeDown",
            Self::ToggleFullscreen => "ToggleFullscreen",
            Self::PlaybackRate(_) => "PlaybackRate",
            Self::Mute(_) => "Mute",
            Self::ToggleMute => "ToggleMute",
            Self::SetAudio(_) => "SetAudio",
            Self::SetSubtitle(_) => "SetSubtitle",
        }
    }
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
    arguments: HashMap<String, Value>,
}

fn argument_f64(arguments: &HashMap<String, Value>, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        arguments.get(*name).and_then(|value| match value {
            Value::Number(value) => value.as_f64(),
            Value::String(value) => value.parse().ok(),
            _ => None,
        })
    })
}

fn argument_i32(arguments: &HashMap<String, Value>, names: &[&str]) -> Option<i32> {
    names.iter().find_map(|name| {
        arguments.get(*name).and_then(|value| match value {
            Value::Number(value) => value.as_i64().and_then(|value| i32::try_from(value).ok()),
            Value::String(value) => value.parse().ok(),
            _ => None,
        })
    })
}

pub enum SocketAction {
    Command(RemoteCommand),
    ForceKeepAlive(u64),
    Ignore(String),
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
            Ok(command.map_or_else(
                || SocketAction::Ignore("Playstate".to_owned()),
                SocketAction::Command,
            ))
        }
        "GeneralCommand" => {
            let request: GeneralCommand = serde_json::from_value(envelope.data)?;
            let command = match request.name.as_str() {
                "SetVolume" => argument_f64(&request.arguments, &["Volume", "volume"])
                    .filter(|value| value.is_finite())
                    .map(RemoteCommand::SetVolume),
                "VolumeUp" => Some(RemoteCommand::VolumeUp),
                "VolumeDown" => Some(RemoteCommand::VolumeDown),
                "ToggleFullscreen" => Some(RemoteCommand::ToggleFullscreen),
                "PlaybackRate" => argument_f64(
                    &request.arguments,
                    &["PlaybackRate", "playbackRate", "rate"],
                )
                .filter(|value| value.is_finite() && *value > 0.0)
                .map(RemoteCommand::PlaybackRate),
                "Mute" => Some(RemoteCommand::Mute(true)),
                "Unmute" => Some(RemoteCommand::Mute(false)),
                "ToggleMute" => Some(RemoteCommand::ToggleMute),
                "SetAudioStreamIndex" => {
                    argument_i32(&request.arguments, &["Index", "AudioStreamIndex", "index"])
                        .map(|index| RemoteCommand::SetAudio((index >= 0).then_some(index)))
                }
                "SetSubtitleStreamIndex" => argument_i32(
                    &request.arguments,
                    &["Index", "SubtitleStreamIndex", "index"],
                )
                .map(|index| RemoteCommand::SetSubtitle((index >= 0).then_some(index))),
                _ => None,
            };
            Ok(command.map_or_else(
                || SocketAction::Ignore("GeneralCommand".to_owned()),
                SocketAction::Command,
            ))
        }
        _ => Ok(SocketAction::Ignore(envelope.message_type)),
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
    #[serde(default)]
    pub trickplay: HashMap<String, Option<HashMap<String, TrickplayInfo>>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
#[serde(rename_all = "PascalCase")]
pub struct TrickplayInfo {
    pub width: u32,
    pub height: u32,
    pub tile_width: u32,
    pub tile_height: u32,
    pub thumbnail_count: u32,
    pub interval: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSegment {
    #[serde(rename = "Type")]
    pub segment_type: Option<String>,
    pub start_ticks: Option<i64>,
    pub end_ticks: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSegmentQueryResult {
    #[serde(default)]
    pub items: Vec<MediaSegment>,
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
    pub bitrate: Option<i64>,
    #[serde(default)]
    pub supports_direct_play: bool,
    pub transcoding_url: Option<String>,
    pub live_stream_id: Option<String>,
    pub default_audio_stream_index: Option<i32>,
    pub default_subtitle_stream_index: Option<i32>,
    #[serde(default)]
    pub media_streams: Vec<MediaStream>,
    #[serde(default)]
    pub required_http_headers: HashMap<String, Option<String>>,
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

        let numeric_volume = r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetVolume","Arguments":{"Volume":37}}}"#;
        assert!(matches!(
            parse_socket_message(numeric_volume).unwrap(),
            SocketAction::Command(RemoteCommand::SetVolume(37.0))
        ));

        let rate = r#"{"MessageType":"GeneralCommand","Data":{"Name":"PlaybackRate","Arguments":{"PlaybackRate":1.25}}}"#;
        assert!(matches!(
            parse_socket_message(rate).unwrap(),
            SocketAction::Command(RemoteCommand::PlaybackRate(1.25))
        ));

        let mute = r#"{"MessageType":"GeneralCommand","Data":{"Name":"Mute","Arguments":{}}}"#;
        assert!(matches!(
            parse_socket_message(mute).unwrap(),
            SocketAction::Command(RemoteCommand::Mute(true))
        ));

        for (command, expected) in [
            ("VolumeUp", RemoteCommand::VolumeUp),
            ("VolumeDown", RemoteCommand::VolumeDown),
            ("ToggleFullscreen", RemoteCommand::ToggleFullscreen),
        ] {
            let message = format!(
                r#"{{"MessageType":"GeneralCommand","Data":{{"Name":"{command}","Arguments":{{}}}}}}"#
            );
            assert!(matches!(
                parse_socket_message(&message).unwrap(),
                SocketAction::Command(actual) if actual.name() == expected.name()
            ));
        }
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

    #[test]
    fn parses_audio_selection_and_disable() {
        let select = r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetAudioStreamIndex","Arguments":{"Index":"3"}}}"#;
        assert!(matches!(
            parse_socket_message(select).unwrap(),
            SocketAction::Command(RemoteCommand::SetAudio(Some(3)))
        ));

        let disable = r#"{"MessageType":"GeneralCommand","Data":{"Name":"SetAudioStreamIndex","Arguments":{"Index":"-1"}}}"#;
        assert!(matches!(
            parse_socket_message(disable).unwrap(),
            SocketAction::Command(RemoteCommand::SetAudio(None))
        ));
    }
}
