use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use image::{ImageFormat, RgbImage};
use tokio::{sync::watch, task::JoinHandle, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{
    jellyfin::JellyfinClient,
    mpv::{MpvHandle, MpvPreviewRequest, Overlay},
    protocol::{ItemDto, TrickplayInfo},
};

const PREFERRED_THUMBNAIL_WIDTH: u32 = 320;
const OVERLAY_ID: u32 = 47;
const TILE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct TrickplayProfile {
    item_id: String,
    media_source_id: String,
    info: TrickplayInfo,
}

impl TrickplayProfile {
    pub fn display_size(&self) -> (u32, u32) {
        (self.info.width, self.info.height)
    }
}

pub fn select_trickplay(item: &ItemDto, media_source_id: &str) -> Option<TrickplayProfile> {
    let variants = item
        .trickplay
        .get(media_source_id)
        .and_then(Option::as_ref)
        .or_else(|| item.trickplay.get(&item.id).and_then(Option::as_ref))
        .or_else(|| {
            (item.trickplay.len() == 1)
                .then(|| item.trickplay.values().next().and_then(Option::as_ref))
                .flatten()
        })?;

    let mut valid = variants
        .values()
        .filter(|info| valid_info(info))
        .cloned()
        .collect::<Vec<_>>();
    valid.sort_unstable_by_key(|info| {
        (
            info.width.abs_diff(PREFERRED_THUMBNAIL_WIDTH),
            std::cmp::Reverse(info.width),
        )
    });
    let info = valid.into_iter().next()?;
    Some(TrickplayProfile {
        item_id: item.id.clone(),
        media_source_id: media_source_id.to_owned(),
        info,
    })
}

fn valid_info(info: &TrickplayInfo) -> bool {
    info.width > 0
        && info.height > 0
        && info.tile_width > 0
        && info.tile_height > 0
        && info.thumbnail_count > 0
        && info.interval > 0
}

pub struct ThumbnailWorker {
    sender: watch::Sender<Option<MpvPreviewRequest>>,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
    directory: PathBuf,
}

impl ThumbnailWorker {
    pub async fn start(
        client: Arc<JellyfinClient>,
        mpv: MpvHandle,
        profile: TrickplayProfile,
    ) -> Result<Self> {
        let directory = std::env::temp_dir().join(format!("jellyfin-mpv-thumbnails-{}", mpv.id));
        tokio::fs::create_dir(&directory)
            .await
            .with_context(|| format!("创建缩略图临时目录失败: {}", directory.display()))?;
        let (sender, receiver) = watch::channel(None);
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let worker_directory = directory.clone();
        let task = tokio::spawn(async move {
            run_worker(
                client,
                mpv,
                profile,
                receiver,
                worker_cancellation,
                worker_directory,
            )
            .await;
        });
        Ok(Self {
            sender,
            cancellation,
            task,
            directory,
        })
    }

    pub fn request(&self, request: Option<MpvPreviewRequest>) {
        self.sender.send_replace(request);
    }

    pub async fn stop(self) {
        self.cancellation.cancel();
        let _ = self.task.await;
        if let Err(error) = tokio::fs::remove_dir_all(&self.directory).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(%error, path = %self.directory.display(), "清理缩略图临时目录失败");
        }
    }
}

struct CachedTile {
    index: u32,
    image: RgbImage,
}

async fn run_worker(
    client: Arc<JellyfinClient>,
    mpv: MpvHandle,
    profile: TrickplayProfile,
    mut receiver: watch::Receiver<Option<MpvPreviewRequest>>,
    cancellation: CancellationToken,
    directory: PathBuf,
) {
    let output = directory.join("preview.bgra");
    let mut cache: Option<CachedTile> = None;
    loop {
        tokio::select! {
            () = cancellation.cancelled() => break,
            changed = receiver.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }

        let request = receiver.borrow_and_update().clone();
        let Some(request) = request else {
            let _ = mpv.remove_overlay(OVERLAY_ID).await;
            continue;
        };
        if let Err(error) = render_preview(
            &client, &mpv, &profile, &request, &receiver, &output, &mut cache,
        )
        .await
        {
            warn!(%error, "显示 Jellyfin Trickplay 缩略图失败");
            let _ = mpv.remove_overlay(OVERLAY_ID).await;
        }
    }
    let _ = mpv.remove_overlay(OVERLAY_ID).await;
}

async fn render_preview(
    client: &JellyfinClient,
    mpv: &MpvHandle,
    profile: &TrickplayProfile,
    request: &MpvPreviewRequest,
    receiver: &watch::Receiver<Option<MpvPreviewRequest>>,
    output: &PathBuf,
    cache: &mut Option<CachedTile>,
) -> Result<()> {
    let location = frame_location(&profile.info, request.seconds)?;
    if cache.as_ref().map(|tile| tile.index) != Some(location.tile_index) {
        let bytes = timeout(
            TILE_DOWNLOAD_TIMEOUT,
            client.trickplay_tile(
                &profile.item_id,
                &profile.media_source_id,
                profile.info.width,
                location.tile_index,
            ),
        )
        .await
        .context("下载 Trickplay 雪碧图超时")??
        .ok_or_else(|| anyhow!("Jellyfin 没有返回雪碧图 {}", location.tile_index))?;
        if receiver.has_changed().unwrap_or(false) {
            return Ok(());
        }
        let image = tokio::task::spawn_blocking(move || decode_jpeg(&bytes))
            .await
            .context("Trickplay JPEG 解码任务异常退出")??;
        *cache = Some(CachedTile {
            index: location.tile_index,
            image,
        });
    }

    if receiver.has_changed().unwrap_or(false) {
        return Ok(());
    }
    let tile = cache.as_ref().expect("雪碧图缓存已准备");
    let pixels = crop_bgra(&tile.image, &profile.info, location)?;
    tokio::fs::write(output, pixels)
        .await
        .context("写入 mpv 缩略图像素失败")?;
    if receiver.has_changed().unwrap_or(false) {
        return Ok(());
    }

    let (x, y, width, height) = fit_destination(request, &profile.info);
    mpv.add_overlay(Overlay {
        id: OVERLAY_ID,
        x,
        y,
        path: output.clone(),
        width: profile.info.width,
        height: profile.info.height,
        display_width: width,
        display_height: height,
    })
    .await?;
    debug!(
        frame = location.frame,
        tile = location.tile_index,
        "Trickplay 缩略图已显示"
    );
    Ok(())
}

fn decode_jpeg(bytes: &[u8]) -> Result<RgbImage> {
    Ok(
        image::load_from_memory_with_format(bytes, ImageFormat::Jpeg)
            .context("解码 Trickplay JPEG 失败")?
            .to_rgb8(),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameLocation {
    frame: u32,
    tile_index: u32,
    column: u32,
    row: u32,
}

fn frame_location(info: &TrickplayInfo, seconds: f64) -> Result<FrameLocation> {
    if !valid_info(info) {
        bail!("Trickplay 元数据无效");
    }
    let raw_frame = if seconds.is_finite() {
        (seconds.max(0.0) * 1000.0 / info.interval as f64).floor() as u64
    } else {
        0
    };
    let frame = raw_frame.min(u64::from(info.thumbnail_count - 1)) as u32;
    let per_tile = info
        .tile_width
        .checked_mul(info.tile_height)
        .ok_or_else(|| anyhow!("Trickplay 雪碧图尺寸溢出"))?;
    let within_tile = frame % per_tile;
    Ok(FrameLocation {
        frame,
        tile_index: frame / per_tile,
        column: within_tile % info.tile_width,
        row: within_tile / info.tile_width,
    })
}

fn crop_bgra(tile: &RgbImage, info: &TrickplayInfo, location: FrameLocation) -> Result<Vec<u8>> {
    let left = location
        .column
        .checked_mul(info.width)
        .ok_or_else(|| anyhow!("Trickplay 裁切坐标溢出"))?;
    let top = location
        .row
        .checked_mul(info.height)
        .ok_or_else(|| anyhow!("Trickplay 裁切坐标溢出"))?;
    if left + info.width > tile.width() || top + info.height > tile.height() {
        bail!(
            "Trickplay 雪碧图尺寸 {}x{} 小于预期裁切区域",
            tile.width(),
            tile.height()
        );
    }

    let pixel_count = info
        .width
        .checked_mul(info.height)
        .ok_or_else(|| anyhow!("Trickplay 缩略图尺寸溢出"))?;
    let mut output = Vec::with_capacity(pixel_count as usize * 4);
    for y in top..top + info.height {
        for x in left..left + info.width {
            let pixel = tile.get_pixel(x, y).0;
            output.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
    }
    Ok(output)
}

fn fit_destination(request: &MpvPreviewRequest, info: &TrickplayInfo) -> (i64, i64, u32, u32) {
    if request.width == 0 || request.height == 0 {
        return (request.x, request.y, info.width, info.height);
    }
    let scale =
        (request.width as f64 / info.width as f64).min(request.height as f64 / info.height as f64);
    let width = (info.width as f64 * scale).round().max(1.0) as u32;
    let height = (info.height as f64 * scale).round().max(1.0) as u32;
    let x = request.x + i64::from(request.width.saturating_sub(width) / 2);
    let y = request.y + i64::from(request.height.saturating_sub(height) / 2);
    (x, y, width, height)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use image::{Rgb, RgbImage};

    use super::*;

    fn info(width: u32) -> TrickplayInfo {
        TrickplayInfo {
            width,
            height: 90,
            tile_width: 4,
            tile_height: 3,
            thumbnail_count: 25,
            interval: 10_000,
        }
    }

    #[test]
    fn selects_the_closest_valid_width_for_the_active_media_source() {
        let item = ItemDto {
            id: "item-1".to_owned(),
            name: "Movie".to_owned(),
            media_type: Some("Video".to_owned()),
            trickplay: HashMap::from([
                (
                    "source-1".to_owned(),
                    Some(HashMap::from([
                        ("160".to_owned(), info(160)),
                        ("320".to_owned(), info(320)),
                        ("640".to_owned(), info(640)),
                    ])),
                ),
                (
                    "source-2".to_owned(),
                    Some(HashMap::from([("200".to_owned(), info(200))])),
                ),
            ]),
        };

        let selected = select_trickplay(&item, "source-1").unwrap();
        assert_eq!(selected.info.width, 320);
        assert_eq!(selected.media_source_id, "source-1");
    }

    #[test]
    fn maps_hover_time_to_tile_and_cell_and_clamps_the_end() {
        let info = info(160);
        assert_eq!(
            frame_location(&info, 130.0).unwrap(),
            FrameLocation {
                frame: 13,
                tile_index: 1,
                column: 1,
                row: 0,
            }
        );
        assert_eq!(frame_location(&info, 9999.0).unwrap().frame, 24);
    }

    #[test]
    fn crops_and_converts_rgb_to_bgra() {
        let mut info = info(2);
        info.height = 1;
        info.tile_width = 2;
        info.tile_height = 1;
        let mut tile = RgbImage::new(4, 1);
        tile.put_pixel(2, 0, Rgb([10, 20, 30]));
        tile.put_pixel(3, 0, Rgb([40, 50, 60]));
        let pixels = crop_bgra(
            &tile,
            &info,
            FrameLocation {
                frame: 1,
                tile_index: 0,
                column: 1,
                row: 0,
            },
        )
        .unwrap();
        assert_eq!(pixels, [30, 20, 10, 255, 60, 50, 40, 255]);
    }

    #[test]
    fn fits_thumbnail_inside_the_osc_preview_area() {
        let request = MpvPreviewRequest {
            seconds: 5.0,
            x: 100,
            y: 200,
            width: 200,
            height: 200,
        };
        assert_eq!(fit_destination(&request, &info(160)), (100, 243, 200, 113));
    }
}
