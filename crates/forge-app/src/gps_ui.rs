//! Offline GPS details and explicit user-launched map links.

use std::path::PathBuf;

use crate::app::ZxrawApp;
use crate::media_meta::MediaMeta;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum GpsState {
    None,
    Invalid,
    Coordinates {
        latitude: f64,
        longitude: f64,
        altitude_m: Option<f64>,
        direction_deg: Option<f64>,
    },
}

pub(crate) fn state(metadata: &forge_media::ImageMetadata) -> GpsState {
    if metadata.gps_invalid {
        return GpsState::Invalid;
    }
    match (metadata.gps_lat, metadata.gps_lon) {
        (Some(latitude), Some(longitude)) if valid_coordinates(latitude, longitude) => {
            GpsState::Coordinates {
                latitude,
                longitude,
                altitude_m: metadata.gps_alt_m.filter(|value| value.is_finite()),
                direction_deg: metadata
                    .gps_direction_deg
                    .filter(|value| value.is_finite() && (0.0..=360.0).contains(value)),
            }
        }
        (None, None) => GpsState::None,
        _ => GpsState::Invalid,
    }
}

fn valid_coordinates(latitude: f64, longitude: f64) -> bool {
    latitude.is_finite()
        && longitude.is_finite()
        && (-90.0..=90.0).contains(&latitude)
        && (-180.0..=180.0).contains(&longitude)
}

fn validate_template(template: &str) -> Result<(), &'static str> {
    let template = template.trim();
    let Some((scheme, rest)) = template.split_once("://") else {
        return Err("Map URL must use http or https.");
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err("Map URL must use http or https.");
    }
    if template
        .chars()
        .any(|c| c.is_control() || c.is_whitespace())
        || template.contains('\\')
    {
        return Err("Map URL contains invalid characters.");
    }
    if !template.contains("{lat}") || !template.contains("{lon}") {
        return Err("Map URL must include {lat} and {lon}.");
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty()
        || authority.contains('{')
        || authority.contains('}')
        || template
            .replace("{lat}", "0")
            .replace("{lon}", "0")
            .chars()
            .any(|character| matches!(character, '{' | '}'))
    {
        return Err("Map URL must have a valid http or https host.");
    }
    Ok(())
}

pub(crate) fn map_url(template: &str, latitude: f64, longitude: f64) -> Result<String, String> {
    if !valid_coordinates(latitude, longitude) {
        return Err("Invalid GPS coordinates.".into());
    }
    validate_template(template).map_err(str::to_string)?;
    Ok(template
        .trim()
        .replace("{lat}", &format!("{latitude:.6}"))
        .replace("{lon}", &format!("{longitude:.6}")))
}

fn dms(value: f64, positive: char, negative: char) -> String {
    let absolute = value.abs();
    let mut degrees = absolute.floor() as u32;
    let minutes_value = (absolute - degrees as f64) * 60.0;
    let mut minutes = minutes_value.floor() as u32;
    let mut seconds = ((minutes_value - minutes as f64) * 60.0 * 100.0).round() / 100.0;
    if seconds >= 60.0 {
        seconds = 0.0;
        minutes += 1;
    }
    if minutes >= 60 {
        minutes = 0;
        degrees += 1;
    }
    let hemisphere = if value.is_sign_negative() {
        negative
    } else {
        positive
    };
    format!("{degrees}° {minutes:02}′ {seconds:05.2}″ {hemisphere}")
}

pub(crate) fn coordinate_dms(latitude: f64, longitude: f64) -> Option<(String, String)> {
    valid_coordinates(latitude, longitude)
        .then(|| (dms(latitude, 'N', 'S'), dms(longitude, 'E', 'W')))
}

pub fn panel(app: &mut ZxrawApp, ui: &mut egui::Ui, entry: &forge_core::FileEntry) {
    ui.separator();
    ui.label(egui::RichText::new("GPS location").strong());
    match app.ops.meta.get(entry, ui.ctx()) {
        Some(Ok(MediaMeta::Image(metadata))) => match state(&metadata) {
            GpsState::None => {
                ui.label("No GPS data");
                ui.label(
                    egui::RichText::new("Search missing:gps to find files without coordinates.")
                        .weak(),
                );
            }
            GpsState::Invalid => {
                ui.colored_label(ui.visuals().error_fg_color, "Invalid GPS data");
            }
            GpsState::Coordinates {
                latitude,
                longitude,
                altitude_m,
                direction_deg,
            } => {
                if let Some((latitude_dms, longitude_dms)) = coordinate_dms(latitude, longitude) {
                    egui::Grid::new("gps-coordinates")
                        .num_columns(2)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Latitude").weak());
                            ui.label(format!("{latitude:.6}  ({latitude_dms})"));
                            ui.end_row();
                            ui.label(egui::RichText::new("Longitude").weak());
                            ui.label(format!("{longitude:.6}  ({longitude_dms})"));
                            ui.end_row();
                            if let Some(altitude) = altitude_m {
                                ui.label(egui::RichText::new("Altitude").weak());
                                ui.label(format!("{altitude:.1} m"));
                                ui.end_row();
                            }
                            if let Some(direction) = direction_deg {
                                ui.label(egui::RichText::new("Image direction").weak());
                                ui.label(format!("{direction:.1}°"));
                                ui.end_row();
                            }
                        });
                    ui.horizontal(|ui| {
                        if ui.button("Copy coordinates").clicked() {
                            ui.ctx().copy_text(format!("{latitude:.6}, {longitude:.6}"));
                            app.toast("Coordinates copied");
                        }
                        let url = map_url(&app.settings.map_url_template, latitude, longitude);
                        if ui
                            .add_enabled(url.is_ok(), egui::Button::new("Open in maps"))
                            .clicked()
                        {
                            match url {
                                Ok(url) if app.testhook.active() => {
                                    app.toast(format!("Would open in maps: {url}"));
                                }
                                Ok(url) => {
                                    let target = PathBuf::from(url);
                                    app.ops.spawn_shell("Open in maps", move || {
                                        forge_win::open_default(&target)
                                    });
                                }
                                Err(error) => app.toast_err(error),
                            }
                        }
                    });
                    if let Err(error) = map_url(&app.settings.map_url_template, latitude, longitude)
                    {
                        ui.label(egui::RichText::new(error).weak());
                    }
                }
            }
        },
        Some(Ok(MediaMeta::Video(_))) => {
            ui.label("No GPS data");
        }
        Some(Err(_)) => {
            ui.label("No GPS data");
        }
        None => {
            ui.spinner();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dms_formats_both_hemispheres_and_rounding() {
        assert_eq!(dms(40.446195, 'N', 'S'), "40° 26′ 46.30″ N");
        assert_eq!(dms(-73.9874, 'E', 'W'), "73° 59′ 14.64″ W");
        assert_eq!(dms(0.0, 'N', 'S'), "0° 00′ 00.00″ N");
    }

    #[test]
    fn rejects_non_finite_and_out_of_range_coordinates() {
        assert!(valid_coordinates(90.0, -180.0));
        assert!(!valid_coordinates(f64::NAN, 0.0));
        assert!(!valid_coordinates(90.0001, 0.0));
        assert!(!valid_coordinates(0.0, f64::INFINITY));
        assert!(!valid_coordinates(0.0, -180.1));
    }

    #[test]
    fn metadata_without_gps_is_empty_and_marked_gps_is_invalid() {
        assert_eq!(
            state(&forge_media::ImageMetadata::default()),
            GpsState::None
        );
        let invalid = forge_media::ImageMetadata {
            gps_invalid: true,
            ..Default::default()
        };
        assert_eq!(state(&invalid), GpsState::Invalid);
    }

    #[test]
    fn map_template_requires_http_or_https_and_both_coordinates() {
        assert!(map_url("https://maps.example/?q={lat},{lon}", 1.25, -2.5).is_ok());
        assert!(map_url("file:///map/{lat}/{lon}", 1.0, 2.0).is_err());
        assert!(map_url("https://maps.example/?q={lat}", 1.0, 2.0).is_err());
        assert!(map_url("https:///map/{lat}/{lon}", 1.0, 2.0).is_err());
    }
}
