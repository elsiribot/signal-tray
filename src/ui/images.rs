//! Images in the chat: thumbnails in the bubbles, and reading images to
//! send from files and the clipboard.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4_layer_shell::LayerShell as _;

use crate::model::OutgoingImage;

/// Thumbnails fit a square of this many logical pixels.
const THUMBNAIL_SIZE: i32 = 220;
/// Staged images are previewed as squares of this size.
pub const STAGED_SIZE: i32 = 56;
/// Signal's size limit for attachments.
const MAX_IMAGE_BYTES: usize = 100 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ImageView {
    pub state: ImageState,
    pub file_name: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Clone, Debug)]
pub enum ImageState {
    /// Not downloaded yet.
    Loading,
    Ready(PathBuf),
    /// The download failed.
    Unavailable,
    /// Being sent; shown from memory.
    Sending(Option<gdk::Texture>),
}

/// An image read to be sent, with its preview.
#[derive(Clone, Debug)]
pub struct StagedImage {
    pub image: OutgoingImage,
    pub thumbnail: Option<gdk::Texture>,
}

enum Thumbnail {
    /// Being decoded; the slots to fill once it is.
    Loading(Vec<glib::WeakRef<gtk::Box>>),
    Ready(gdk::Texture),
    /// GTK can't decode it (e.g. HEIC); it's offered as a link instead.
    Failed,
}

/// Thumbnails of the open chat, decoded in the background, by file.
#[derive(Default)]
pub struct Thumbnails(RefCell<HashMap<PathBuf, Thumbnail>>);

impl Thumbnails {
    /// Forgets the thumbnails, e.g. when switching chats.
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    /// A widget showing `image` in a bubble.
    pub fn widget(self: &Rc<Self>, image: &ImageView) -> gtk::Widget {
        let slot = gtk::Box::builder()
            .halign(gtk::Align::Start)
            .css_classes(["image"])
            .build();
        let (width, height) = fit(image.width, image.height);
        match &image.state {
            ImageState::Loading => slot.append(&placeholder(width, height, true)),
            ImageState::Sending(Some(texture)) => slot.append(&picture(texture, width, height)),
            ImageState::Sending(None) => slot.append(&placeholder(width, height, false)),
            ImageState::Unavailable => {
                slot.append(&label("🖼 Image not available", &["dim"]));
            }
            ImageState::Ready(path) => {
                open_on_click(&slot, path);
                let mut thumbnails = self.0.borrow_mut();
                match thumbnails.get_mut(path) {
                    Some(Thumbnail::Ready(texture)) => fill(&slot, Some(texture), image),
                    Some(Thumbnail::Failed) => fill(&slot, None, image),
                    Some(Thumbnail::Loading(waiting)) => {
                        slot.append(&placeholder(width, height, true));
                        waiting.push(slot.downgrade());
                    }
                    None => {
                        slot.append(&placeholder(width, height, true));
                        thumbnails.insert(path.clone(), Thumbnail::Loading(vec![slot.downgrade()]));
                        let size = THUMBNAIL_SIZE * slot.scale_factor();
                        glib::spawn_future_local(self.clone().load(
                            path.clone(),
                            size,
                            image.clone(),
                        ));
                    }
                }
            }
        }
        slot.upcast()
    }

    async fn load(self: Rc<Self>, path: PathBuf, size: i32, image: ImageView) {
        let decoding = path.clone();
        let texture = match gio::spawn_blocking(move || thumbnail(&decoding, size)).await {
            Ok(Ok(texture)) => Some(texture),
            Ok(Err(err)) => {
                tracing::info!(%err, path = %path.display(), "can't show image");
                None
            }
            Err(_) => None,
        };
        let waiting = {
            let mut thumbnails = self.0.borrow_mut();
            // Gone if the chat was switched meanwhile.
            let Some(Thumbnail::Loading(waiting)) = thumbnails.get_mut(&path) else {
                return;
            };
            let waiting = std::mem::take(waiting);
            thumbnails.insert(
                path,
                texture.clone().map_or(Thumbnail::Failed, Thumbnail::Ready),
            );
            waiting
        };
        for slot in waiting.iter().filter_map(glib::WeakRef::upgrade) {
            fill(&slot, texture.as_ref(), &image);
        }
    }
}

/// Decodes `path` to fit a `size` square, never enlarging it.
fn thumbnail(path: &Path, size: i32) -> Result<gdk::Texture, glib::Error> {
    let fits = Pixbuf::file_info(path).is_some_and(|(_, w, h)| w <= size && h <= size);
    let pixbuf = if fits {
        Pixbuf::from_file(path)?
    } else {
        Pixbuf::from_file_at_scale(path, size, size, true)?
    };
    let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
    Ok(gdk::Texture::for_pixbuf(&pixbuf))
}

/// Replaces the slot's content with the decoded thumbnail, or a link to
/// the file if it couldn't be decoded.
fn fill(slot: &gtk::Box, texture: Option<&gdk::Texture>, image: &ImageView) {
    while let Some(child) = slot.first_child() {
        slot.remove(&child);
    }
    match texture {
        Some(texture) => {
            let scale = slot.scale_factor();
            let (width, height) = fit(
                u32::try_from(texture.width() / scale).ok(),
                u32::try_from(texture.height() / scale).ok(),
            );
            slot.append(&picture(texture, width, height));
        }
        None => {
            let name = image.file_name.as_deref().unwrap_or("Image");
            slot.append(&label(&format!("🖼 {name}"), &["image-link"]));
        }
    }
}

fn open_on_click(slot: &gtk::Box, path: &Path) {
    slot.set_cursor_from_name(Some("pointer"));
    slot.set_tooltip_text(Some("Open"));
    let click = gtk::GestureClick::new();
    let path = path.to_owned();
    click.connect_released(move |gesture, _, _, _| {
        let window = gesture
            .widget()
            .and_then(|w| w.root())
            .and_downcast::<gtk::Window>();
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
        let path = path.clone();
        let parent = window.as_ref().and_then(dialog_parent);
        launcher.launch(parent, gio::Cancellable::NONE, move |result| {
            if let Err(err) = result {
                tracing::warn!(%err, path = %path.display(), "opening an image failed");
            }
        });
    });
    slot.add_controller(click);
}

/// The parent to give a dialog or launched app. A layer surface can't be
/// one: compositors disconnect clients that try.
pub fn dialog_parent(window: &gtk::Window) -> Option<&gtk::Window> {
    (!window.is_layer_window()).then_some(window)
}

/// Display size of an image in a bubble; a landscape guess if unknown.
fn fit(width: Option<u32>, height: Option<u32>) -> (i32, i32) {
    let (Some(width), Some(height)) = (width.filter(|w| *w > 0), height.filter(|h| *h > 0)) else {
        return (THUMBNAIL_SIZE, THUMBNAIL_SIZE * 3 / 4);
    };
    let (width, height) = (f64::from(width), f64::from(height));
    let scale = (f64::from(THUMBNAIL_SIZE) / width.max(height)).min(1.0);
    // Very narrow images stay visible.
    let px = |v: f64| (v * scale).round().max(24.0) as i32;
    (px(width), px(height))
}

/// `texture` at exactly `width`×`height`, cropped to fill. A picture
/// alone asks for its texture's full size, so it's laid over a box of the
/// wanted size instead.
pub fn picture(texture: &gdk::Texture, width: i32, height: i32) -> gtk::Widget {
    let frame = gtk::Overlay::builder()
        .child(
            &gtk::Box::builder()
                .width_request(width)
                .height_request(height)
                .build(),
        )
        .overflow(gtk::Overflow::Hidden)
        .css_classes(["thumbnail"])
        .build();
    frame.add_overlay(
        &gtk::Picture::builder()
            .paintable(texture)
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Cover)
            .build(),
    );
    frame.upcast()
}

fn placeholder(width: i32, height: i32, busy: bool) -> gtk::Widget {
    let placeholder = gtk::Box::builder()
        .width_request(width)
        .height_request(height)
        .css_classes(["image-placeholder"])
        .build();
    if busy {
        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .hexpand(true)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        placeholder.append(&spinner);
    }
    placeholder.upcast()
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(classes.to_vec())
        .build()
}

/// Reads an image file to send. The error is for the user.
pub async fn read_file(file: gio::File) -> Result<StagedImage, String> {
    let name = file
        .basename()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.uri().to_string());
    let (data, _) = file
        .load_contents_future()
        .await
        .map_err(|err| format!("Couldn't read {name}: {err}"))?;
    let (content_type, _) = gio::content_type_guess(Some(&name), Some(data.as_ref()));
    let mime = gio::content_type_get_mime_type(&content_type)
        .map(|m| m.to_string())
        .unwrap_or_default();
    if !mime.starts_with("image/") {
        return Err(format!("{name} isn't an image; only images can be sent."));
    }
    stage(Arc::from(data.as_ref()), mime, Some(name)).await
}

/// Takes a pasted or dropped image, as PNG.
pub async fn read_texture(texture: gdk::Texture) -> Result<StagedImage, String> {
    let encoding = texture.clone();
    let png = gio::spawn_blocking(move || encoding.save_to_png_bytes())
        .await
        .map_err(|_| "Couldn't read the pasted image.".to_owned())?;
    check_size(png.len(), "The image")?;
    Ok(StagedImage {
        image: OutgoingImage {
            content_type: "image/png".to_owned(),
            file_name: None,
            width: u32::try_from(texture.width()).ok(),
            height: u32::try_from(texture.height()).ok(),
            data: Arc::from(png.as_ref()),
        },
        thumbnail: Some(texture),
    })
}

async fn stage(
    data: Arc<[u8]>,
    content_type: String,
    file_name: Option<String>,
) -> Result<StagedImage, String> {
    check_size(data.len(), file_name.as_deref().unwrap_or("The image"))?;
    let decoding = data.clone();
    let decoded = gio::spawn_blocking(move || preview(decoding)).await;
    // Undecodable images (e.g. HEIC without a loader) are still sent.
    let (width, height, thumbnail) = match decoded {
        Ok(Ok((width, height, thumbnail))) => (Some(width), Some(height), Some(thumbnail)),
        Ok(Err(err)) => {
            tracing::info!(%err, "can't preview image");
            (None, None, None)
        }
        Err(_) => (None, None, None),
    };
    Ok(StagedImage {
        image: OutgoingImage {
            content_type,
            file_name,
            width,
            height,
            data,
        },
        thumbnail,
    })
}

fn check_size(len: usize, what: &str) -> Result<(), String> {
    if len > MAX_IMAGE_BYTES {
        return Err(format!(
            "{what} is too large to send ({} MB, at most {}).",
            len / 1_000_000,
            MAX_IMAGE_BYTES / 1_000_000
        ));
    }
    Ok(())
}

/// The image's upright dimensions and a small texture of it.
fn preview(data: Arc<[u8]>) -> Result<(u32, u32, gdk::Texture), glib::Error> {
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(data));
    let pixbuf = Pixbuf::from_stream(&stream, gio::Cancellable::NONE)?;
    let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
    let (width, height) = (pixbuf.width(), pixbuf.height());
    // Room for HiDPI.
    let size = f64::from(STAGED_SIZE * 2);
    let scale = (size / f64::from(width.min(height))).min(1.0);
    let small = pixbuf
        .scale_simple(
            ((f64::from(width) * scale).round() as i32).max(1),
            ((f64::from(height) * scale).round() as i32).max(1),
            gtk::gdk_pixbuf::InterpType::Bilinear,
        )
        .unwrap_or(pixbuf);
    Ok((
        u32::try_from(width).unwrap_or_default(),
        u32::try_from(height).unwrap_or_default(),
        gdk::Texture::for_pixbuf(&small),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_fit_the_square_and_keep_their_shape() {
        assert_eq!(fit(Some(4000), Some(3000)), (220, 165));
        assert_eq!(fit(Some(1080), Some(1920)), (124, 220));
        assert_eq!(fit(Some(100), Some(50)), (100, 50), "never enlarged");
        assert_eq!(fit(Some(3000), Some(10)), (220, 24), "slivers stay visible");
        assert_eq!(fit(None, Some(10)), (220, 165));
        assert_eq!(fit(Some(0), Some(0)), (220, 165));
    }
}
