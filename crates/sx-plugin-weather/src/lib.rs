//! # `sx-plugin-weather`
//!
//! Weather handler for `sx` — wraps the [`open-meteo-rs`] crate.
//!
//! ## Query syntax
//!
//! ```text
//! weather:berlin
//! weather:Jettingen, Germany
//! weather:48.1,11.6
//! ```
//!
//! City names are geocoded via the open-meteo geocoding API.
//! `lat,lon` coordinates skip geocoding and go straight to the forecast API.

use open_meteo_rs::{
    forecast::{Options as ForecastOptions, WindSpeedUnit},
    geocoding::Options as GeoOptions,
    Client, Location,
};
use sx_plugin_api::{
    stabby, SxBytes, SxHandlerInfo, SxMetaEntry, SxNamespacePolicy, SxQueryContext,
    SxResult, SxResultSender,
};

/// Parse `"lat,lon"` and return `(lat, lon)`, or `None` for non-numeric input.
fn parse_coordinates(s: &str) -> Option<(f64, f64)> {
    let s = s.trim();
    let comma = s.find(',')?;
    let lat = s[..comma].trim().parse::<f64>().ok()?;
    let lon = s[comma + 1..].trim().parse::<f64>().ok()?;
    if lat.abs() <= 90.0 && lon.abs() <= 180.0 {
        Some((lat, lon))
    } else {
        None
    }
}

/// Map a WMO weather interpretation code to a short English description.
fn wmo_description(code: i64) -> &'static str {
    match code {
        0 => "clear sky",
        1 => "mainly clear",
        2 => "partly cloudy",
        3 => "overcast",
        45 | 48 => "foggy",
        51 => "light drizzle",
        53 => "moderate drizzle",
        55 => "dense drizzle",
        61 => "slight rain",
        63 => "moderate rain",
        65 => "heavy rain",
        66 | 67 => "freezing rain",
        71 => "slight snow",
        73 => "moderate snow",
        75 => "heavy snow",
        77 => "snow grains",
        80 => "slight showers",
        81 => "moderate showers",
        82 => "violent showers",
        85 | 86 => "snow showers",
        95 => "thunderstorm",
        96 | 99 => "thunderstorm with hail",
        _ => "unknown",
    }
}

/// Map a WMO weather code + day/night flag to a symbol present in Adwaita Sans/Mono,
/// the font GPUI uses on this system.
fn wmo_emoji(code: i64, is_day: bool) -> &'static str {
    match code {
        0          => if is_day { "\u{2600}" } else { "\u{2605}" }, // ☀ / ★
        1 | 2      => "\u{263C}",  // ☼  (sun with rays = partly cloudy approximation)
        3          => "\u{25CF}",  // ●  (filled circle = overcast)
        45 | 48    => "\u{25CE}",  // ◎  (bull's-eye = fog)
        51..=55    => "\u{25CC}",  // ◌  (dotted circle = drizzle)
        61..=67 | 80..=82 => "\u{25CB}", // ○  (hollow circle = rain)
        71..=77 | 85 | 86 => "\u{25C7}", // ◇  (diamond = snow)
        95 | 96 | 99 => "\u{26A1}", // ⚡  (lightning = thunderstorm)
        _ => if is_day { "\u{2600}" } else { "\u{2605}" },
    }
}

fn send_error(sender: &SxResultSender, msg: &str) {
    let mut entries = stabby::vec::Vec::new();
    entries.push(SxMetaEntry::text("error", msg));
    sender.send(SxResult {
        metadata: entries,
        ..SxResult::new("weather", 0.0)
    });
}

pub struct WeatherHandler;

impl WeatherHandler {
    pub fn new() -> Self {
        let _ = env_logger::try_init();
        Self
    }

    fn do_query(&self, value: &str, sender: &SxResultSender) {
        log::debug!("weather: query({:?})", value);

        // The plugin is called on a spawn_blocking thread, so we can block.
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                send_error(sender, &format!("failed to create tokio runtime: {e}"));
                return;
            }
        };

        rt.block_on(async { self.run_query(value, sender).await });
    }

    async fn run_query(&self, value: &str, sender: &SxResultSender) {
        let client = Client::new();

        // ── resolve lat/lon ───────────────────────────────────────────────────
        let (lat, lon, location_name) = if let Some((lat, lon)) = parse_coordinates(value) {
            log::debug!("weather: parsed coordinates lat={lat:.4}, lon={lon:.4}");
            (lat, lon, format!("{lat:.2},{lon:.2}"))
        } else {
            let opts = GeoOptions::default()
                .with_name(value.to_owned())
                .with_language("en".to_owned())
                .with_count(1);

            let geo = match client.geocoding(opts).await {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("weather: geocoding failed for {:?}: {e}", value);
                    send_error(sender, &format!("geocoding failed: {e}"));
                    return;
                }
            };

            let result = match geo.results.and_then(|mut v| if v.is_empty() { None } else { Some(v.remove(0)) }) {
                Some(r) => r,
                None => {
                    log::info!("weather: no results for {:?}", value);
                    send_error(sender, &format!("no results found for '{value}'"));
                    return;
                }
            };

            let (lat, lon) = match (result.latitude, result.longitude) {
                (Some(lat), Some(lon)) => (lat, lon),
                _ => {
                    send_error(sender, "geocoding result missing coordinates");
                    return;
                }
            };
            let name = match (&result.name, &result.country) {
                (Some(n), Some(c)) => format!("{n}, {c}"),
                (Some(n), None) => n.clone(),
                _ => value.to_owned(),
            };
            log::debug!("weather: geocoded {:?} → {name}, lat={lat:.4}, lon={lon:.4}", value);
            (lat, lon, name)
        };

        // ── fetch forecast ────────────────────────────────────────────────────
        let mut opts = ForecastOptions::default();
        opts.location = Location { lat, lng: lon };
        opts.wind_speed_unit = Some(WindSpeedUnit::Kmh);
        opts.current.push("temperature_2m".into());
        opts.current.push("wind_speed_10m".into());
        opts.current.push("weather_code".into());
        opts.current.push("is_day".into());

        let forecast = match client.forecast(opts).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("weather: forecast failed for {location_name}: {e}");
                send_error(sender, &format!("forecast failed: {e}"));
                return;
            }
        };

        let current = match forecast.current {
            Some(c) => c,
            None => {
                send_error(sender, "forecast response contained no current data");
                return;
            }
        };

        let get_f64 = |key: &str| -> Option<f64> {
            current.values.get(key)?.value.as_f64()
        };
        let get_i64 = |key: &str| -> Option<i64> {
            current.values.get(key)?.value.as_i64()
        };

        let temp = get_f64("temperature_2m");
        let wind = get_f64("wind_speed_10m");
        let code = get_i64("weather_code").unwrap_or(0);
        let is_day = get_i64("is_day").unwrap_or(1) != 0;

        log::info!(
            "weather: {} — {:?}°C, {}, {:?} km/h",
            location_name, temp, wmo_description(code), wind,
        );

        let mut entries = stabby::vec::Vec::new();
        entries.push(SxMetaEntry::text("location", &location_name));
        entries.push(SxMetaEntry::text("condition", wmo_description(code)));
        if let Some(t) = temp {
            let rounded = (t * 10.0).round() / 10.0;
            entries.push(SxMetaEntry::number("temp_c", rounded));
        }
        if let Some(w) = wind {
            let rounded = (w * 10.0).round() / 10.0;
            entries.push(SxMetaEntry::number("windspeed_kph", rounded));
        }
        entries.push(SxMetaEntry::text("is_day", if is_day { "yes" } else { "no" }));

        let mut result = SxResult {
            title: stabby::option::Option::Some(SxBytes::from_str_bytes(&location_name)),
            metadata: entries,
            ..SxResult::new("weather", 1.0)
        };
        result.metadata.push(SxMetaEntry::text("icon_symbol", wmo_emoji(code, is_day)));

        sender.send(result);
    }
}

impl Default for WeatherHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl sx_plugin_api::SxHandler for WeatherHandler {
    extern "C" fn info(&self) -> SxHandlerInfo {
        SxHandlerInfo {
            handler_version: SxBytes::from_str_bytes(env!("CARGO_PKG_VERSION")),
            namespace: SxBytes::from_str_bytes("weather"),
            aliases: stabby::vec::Vec::new(),
            description: SxBytes::from_str_bytes(
                "Fetch current weather conditions (weather:berlin, weather:48.1,11.6)",
            ),
            value_hint: SxBytes::from_str_bytes("London"),
            file_extensions: stabby::vec::Vec::new(),
            file_extensions_optional: stabby::vec::Vec::new(),
            namespace_policy: SxNamespacePolicy::Exclusive,
            global_scope: false,
            is_file_index: false,
        }
    }

    extern "C" fn query(
        &self,
        value: stabby::str::Str<'_>,
        _ctx: &SxQueryContext,
        sender: SxResultSender,
    ) {
        self.do_query(value.as_ref(), &sender);
    }

    extern "C" fn prepare_extract(
        &self,
    ) -> stabby::option::Option<sx_plugin_api::SxExtractorHandle> {
        stabby::option::Option::None()
    }

    extern "C" fn hints(
        &self,
        _partial_value: stabby::str::Str<'_>,
    ) -> sx_plugin_api::SxHints {
        sx_plugin_api::SxHints::empty()
    }
}

sx_plugin_api::declare_plugin!(WeatherHandler::new());
