use sx_plugin_api::SxHandler as _;
use sx_plugin_weather::WeatherHandler;

#[test]
fn handler_info() {
    let h = WeatherHandler::new();
    let info = h.info();
    assert_eq!(info.namespace.as_str(), "weather");
    assert_eq!(info.aliases.as_slice().len(), 0);
}
