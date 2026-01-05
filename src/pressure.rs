//! Vienna weather pressure fetching module
//!
//! Fetches air pressure data from Vienna weather stations and computes
//! the mean pressure for CO2 sensor compensation.

use anyhow::{Context, Result};
use reqwest::header;
use soup::prelude::*;
use tracing::{info, warn};

/// Vienna weather stations for pressure averaging
const VIENNA_STATIONS: [&str; 3] = ["Wien Donaufeld", "Wien Hohe Warte", "Wien Innere Stadt"];

/// URL of Vienna weather data page
const WEATHER_URL: &str = "https://www.wien.gv.at/svc/weather/measurements";

/// Fetch current air pressure from Vienna weather stations
/// Returns the mean pressure of the configured stations plus the offset
pub async fn fetch_vienna_pressure(pressure_offset: f64) -> Result<f64> {
    info!("Fetching weather data from {}", WEATHER_URL);

    let mut headers = header::HeaderMap::new();
    headers.insert(
        "User-Agent",
        "Mozilla/5.0 (X11; Linux x86_64; rv:146.0) Gecko/20100101 Firefox/146.0"
            .parse()
            .unwrap(),
    );
    headers.insert(
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
            .parse()
            .unwrap(),
    );
    headers.insert("Accept-Language", "en".parse().unwrap());
    headers.insert("DNT", "1".parse().unwrap());
    headers.insert("Sec-GPC", "1".parse().unwrap());
    headers.insert("Connection", "keep-alive".parse().unwrap());
    headers.insert("Upgrade-Insecure-Requests", "1".parse().unwrap());
    headers.insert("Sec-Fetch-Dest", "document".parse().unwrap());
    headers.insert("Sec-Fetch-Mode", "navigate".parse().unwrap());
    headers.insert("Sec-Fetch-Site", "none".parse().unwrap());
    headers.insert("Priority", "u=0, i".parse().unwrap());
    headers.insert("Pragma", "no-cache".parse().unwrap());
    headers.insert("Cache-Control", "no-cache".parse().unwrap());

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let html = client
        .get(WEATHER_URL)
        .headers(headers)
        .send()
        .await?
        .text()
        .await?;

    dbg!(&html);

    parse_vienna_pressure_from_html(&html, pressure_offset)
}

/// Parse pressure data from Vienna weather HTML
/// Separated from fetch_vienna_pressure for testability
pub fn parse_vienna_pressure_from_html(html: &str, pressure_offset: f64) -> Result<f64> {
    let soup = Soup::new(html);

    // Find the Luftdruck column index from header row
    let mut luftdruck_col: Option<usize> = None;

    // Parse table structure - find header row in thead
    for row in soup
        .tag("thead")
        .find_all()
        .flat_map(|thead| thead.tag("tr").find_all())
    {
        let headers: Vec<String> = row
            .tag("th")
            .find_all()
            .map(|th| th.text().trim().to_string())
            .collect();

        for (i, header) in headers.iter().enumerate() {
            if header.contains("Luftdruck") {
                // Subtract 1 because the first column (Ort) is a th in data rows,
                // so Luftdruck at index 6 in header means index 5 in td cells
                luftdruck_col = Some(i.saturating_sub(1));
                break;
            }
        }
        if luftdruck_col.is_some() {
            break;
        }
    }

    let luftdruck_col =
        luftdruck_col.context("Could not find 'Luftdruck' column in weather table")?;

    // Parse data rows in tbody
    let mut pressures: Vec<f64> = Vec::new();

    for row in soup
        .tag("tbody")
        .find_all()
        .flat_map(|tbody| tbody.tag("tr").find_all())
    {
        // Location is in the <th> of each data row
        let location = row
            .tag("th")
            .find()
            .map(|th| th.text().trim().to_string())
            .unwrap_or_default();

        // Data cells are in <td> elements
        let cells: Vec<String> = row
            .tag("td")
            .find_all()
            .map(|td| td.text().trim().to_string())
            .collect();

        // Check if this is one of our target stations
        if VIENNA_STATIONS.iter().any(|s| location.contains(s)) {
            if let Some(pressure_str) = cells.get(luftdruck_col) {
                // Parse pressure like "1015,0 hPa" -> 1015.0
                if let Some(pressure) = parse_pressure(pressure_str) {
                    info!("Found pressure for {}: {} mBar", location, pressure);
                    pressures.push(pressure);
                } else {
                    warn!(
                        "Could not parse pressure '{}' for {}",
                        pressure_str, location
                    );
                }
            }
        }
    }

    if pressures.is_empty() {
        anyhow::bail!("No pressure data found for Vienna stations");
    }

    let mean_pressure: f64 = pressures.iter().sum::<f64>() / pressures.len() as f64;
    let final_pressure = mean_pressure + pressure_offset;

    info!(
        "Mean pressure from {} stations: {:.1} mBar, with offset {:.1}: {:.1} mBar",
        pressures.len(),
        mean_pressure,
        pressure_offset,
        final_pressure
    );

    Ok(final_pressure)
}

/// Parse pressure string like "1015,0 hPa" to f64
pub fn parse_pressure(s: &str) -> Option<f64> {
    // Remove "hPa" suffix and trim
    let s = s.replace("hPa", "").trim().to_string();
    // Replace comma with dot for decimal
    let s = s.replace(',', ".");
    s.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pressure() {
        assert_eq!(parse_pressure("1015,0 hPa"), Some(1015.0));
        assert_eq!(parse_pressure("1013,5 hPa"), Some(1013.5));
        assert_eq!(parse_pressure("999,9 hPa"), Some(999.9));
        assert_eq!(parse_pressure("invalid"), None);
    }

    /// Sample HTML matching the actual Vienna weather page structure
    fn sample_vienna_weather_html() -> &'static str {
        r#"
        <table>
            <thead>
                <tr>
                    <th>Ort</th>
                    <th>Temperatur</th>
                    <th>Luftfeuchte</th>
                    <th>Wind</th>
                    <th>Windstärke, Böen</th>
                    <th>Niederschlag</th>
                    <th>Luftdruck</th>
                </tr>
            </thead>
            <tbody>
                <tr>
                    <th>Wien Donaufeld</th>
                    <td>-2,3 °C</td>
                    <td>69 %</td>
                    <td>Nordost</td>
                    <td>3 km/h, 7 km/h</td>
                    <td>0,0 mm</td>
                    <td>1013,0 hPa</td>
                </tr>
                <tr>
                    <th>Wien Hohe Warte</th>
                    <td>-3,0 °C</td>
                    <td>81 %</td>
                    <td>Nordost</td>
                    <td>8 km/h, 14 km/h</td>
                    <td>0,0 mm</td>
                    <td>1015,0 hPa</td>
                </tr>
                <tr>
                    <th>Wien Innere Stadt</th>
                    <td>-1,9 °C</td>
                    <td>73 %</td>
                    <td>Nordost</td>
                    <td>7 km/h, 13 km/h</td>
                    <td>0,0 mm</td>
                    <td>1014,0 hPa</td>
                </tr>
                <tr>
                    <th>Schwechat Flughafen</th>
                    <td>-3,0 °C</td>
                    <td>72 %</td>
                    <td>Windstille</td>
                    <td>n.v. km/h, 15 km/h</td>
                    <td>0,0 mm</td>
                    <td>1012,0 hPa</td>
                </tr>
            </tbody>
        </table>
        "#
    }

    #[test]
    fn test_parse_vienna_pressure_from_html() {
        let html = sample_vienna_weather_html();
        let result = parse_vienna_pressure_from_html(html, 0.0).unwrap();

        // Mean of 1013.0, 1015.0, 1014.0 = 1014.0
        assert!((result - 1014.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_vienna_pressure_with_offset() {
        let html = sample_vienna_weather_html();
        let result = parse_vienna_pressure_from_html(html, 5.0).unwrap();

        // Mean of 1013.0, 1015.0, 1014.0 = 1014.0, plus offset 5.0 = 1019.0
        assert!((result - 1019.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_vienna_pressure_negative_offset() {
        let html = sample_vienna_weather_html();
        let result = parse_vienna_pressure_from_html(html, -10.0).unwrap();

        // Mean 1014.0 - 10.0 = 1004.0
        assert!((result - 1004.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_vienna_pressure_missing_luftdruck_column() {
        let html = r#"
        <table>
            <thead>
                <tr>
                    <th>Ort</th>
                    <th>Temperatur</th>
                </tr>
            </thead>
            <tbody>
                <tr>
                    <th>Wien Donaufeld</th>
                    <td>15,2 °C</td>
                </tr>
            </tbody>
        </table>
        "#;

        let result = parse_vienna_pressure_from_html(html, 0.0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Luftdruck"));
    }

    #[test]
    fn test_parse_vienna_pressure_no_matching_stations() {
        let html = r#"
        <table>
            <thead>
                <tr>
                    <th>Ort</th>
                    <th>Luftdruck</th>
                </tr>
            </thead>
            <tbody>
                <tr>
                    <th>Salzburg</th>
                    <td>1010,0 hPa</td>
                </tr>
                <tr>
                    <th>Graz</th>
                    <td>1008,0 hPa</td>
                </tr>
            </tbody>
        </table>
        "#;

        let result = parse_vienna_pressure_from_html(html, 0.0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No pressure data"));
    }

    #[test]
    fn test_parse_vienna_pressure_partial_stations() {
        // Only one of the three stations present
        let html = r#"
        <table>
            <thead>
                <tr>
                    <th>Ort</th>
                    <th>Luftdruck</th>
                </tr>
            </thead>
            <tbody>
                <tr>
                    <th>Wien Hohe Warte</th>
                    <td>1020,0 hPa</td>
                </tr>
                <tr>
                    <th>Salzburg</th>
                    <td>1010,0 hPa</td>
                </tr>
            </tbody>
        </table>
        "#;

        let result = parse_vienna_pressure_from_html(html, 0.0).unwrap();
        // Only one station, so mean = 1020.0
        assert!((result - 1020.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_vienna_pressure_invalid_pressure_value() {
        let html = r#"
        <table>
            <thead>
                <tr>
                    <th>Ort</th>
                    <th>Luftdruck</th>
                </tr>
            </thead>
            <tbody>
                <tr>
                    <th>Wien Donaufeld</th>
                    <td>---</td>
                </tr>
                <tr>
                    <th>Wien Hohe Warte</th>
                    <td>1015,0 hPa</td>
                </tr>
            </tbody>
        </table>
        "#;

        let result = parse_vienna_pressure_from_html(html, 0.0).unwrap();
        // Only Wien Hohe Warte has valid data
        assert!((result - 1015.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_vienna_pressure_empty_table() {
        let html = r#"
        <table>
            <thead>
                <tr>
                    <th>Ort</th>
                    <th>Luftdruck</th>
                </tr>
            </thead>
            <tbody>
            </tbody>
        </table>
        "#;

        let result = parse_vienna_pressure_from_html(html, 0.0);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_vienna_pressure_no_table() {
        let html = "<html><body><p>No weather data</p></body></html>";

        let result = parse_vienna_pressure_from_html(html, 0.0);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_vienna_pressure_from_fixture_file() {
        let html = include_str!("../tests/fixtures/vienna_weather.html");
        let result = parse_vienna_pressure_from_html(html, 0.0).unwrap();

        // Should find all 3 Vienna stations and compute mean
        // Based on fixture: Wien Donaufeld 1015.0, Wien Hohe Warte 1015.1, Wien Innere Stadt 1015.0
        // Mean = (1015.0 + 1015.1 + 1015.0) / 3 ≈ 1015.03
        assert!(
            result > 1000.0 && result < 1100.0,
            "Pressure {} should be reasonable",
            result
        );
    }

    #[test]
    fn test_parse_vienna_pressure_from_fixture_file_with_offset() {
        let html = include_str!("../tests/fixtures/vienna_weather.html");
        let base_result = parse_vienna_pressure_from_html(html, 0.0).unwrap();
        let offset_result = parse_vienna_pressure_from_html(html, 10.5).unwrap();

        assert!((offset_result - base_result - 10.5).abs() < 0.01);
    }
}
