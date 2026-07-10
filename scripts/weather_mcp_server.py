#!/usr/bin/env python3
"""Weather MCP server (JSON-RPC 2.0 over stdio) using Open-Meteo API.

Capabilities (callable as `mcp__weather__<tool>`):
  status              — self-check: confirms connectivity to Open-Meteo
  get_current_weather — current conditions for any city worldwide
  get_forecast        — multi-day forecast with daily min/max, rain %, sunrise/sunset
  get_air_quality     — air quality index, pollutants, health advice

Data sources (free, no API key required):
  - Open-Meteo Forecast API (weather, forecast)
  - Open-Meteo Air Quality API (air quality)
  - Open-Meteo Geocoding API (city name → coordinates)

Run with:
  python3 scripts/weather_mcp_server.py
"""

from __future__ import annotations

import json
import sys
from typing import Any

import httpx

# ── Logging ─────────────────────────────────────────────────────────────────────

def log(msg: str) -> None:
    print(f"[weather_mcp] {msg}", file=sys.stderr, flush=True)


# ── Constants ───────────────────────────────────────────────────────────────────

GEOCODING_URL = "https://geocoding-api.open-meteo.com/v1/search"
FORECAST_URL  = "https://api.open-meteo.com/v1/forecast"
AIR_URL       = "https://air-quality-api.open-meteo.com/v1/air-quality"

COMMON_HEADERS = {
    "User-Agent": "SkaldWeatherMCP/1.0",
    "Accept": "application/json",
}

HTTP_TIMEOUT = 10.0

# WMO weather codes → human-readable description
WMO_CODES: dict[int, str] = {
    0:  "Clear sky",
    1:  "Mainly clear",
    2:  "Partly cloudy",
    3:  "Overcast",
    45: "Fog",
    48: "Depositing rime fog",
    51: "Light drizzle",
    53: "Moderate drizzle",
    55: "Dense drizzle",
    56: "Light freezing drizzle",
    57: "Dense freezing drizzle",
    61: "Slight rain",
    63: "Moderate rain",
    65: "Heavy rain",
    66: "Light freezing rain",
    67: "Heavy freezing rain",
    71: "Slight snowfall",
    73: "Moderate snowfall",
    75: "Heavy snowfall",
    77: "Snow grains",
    80: "Slight rain showers",
    81: "Moderate rain showers",
    82: "Violent rain showers",
    85: "Slight snow showers",
    86: "Heavy snow showers",
    95: "Thunderstorm",
    96: "Thunderstorm with slight hail",
    99: "Thunderstorm with heavy hail",
}

# ── Helpers ─────────────────────────────────────────────────────────────────────

def _wmo_desc(code: int | None) -> str:
    """Convert WMO weather code to human-readable text."""
    if code is None:
        return "Unknown"
    return WMO_CODES.get(code, f"Unknown ({code})")


def _aqi_label_eu(value: Any) -> str:
    """European AQI band name. Open-Meteo returns a numeric EAQI on the
    0–100+ scale: 0–20 Good, 20–40 Fair, 40–60 Moderate, 60–80 Poor,
    80–100 Very poor, >100 Extremely poor."""
    v = _num(value)
    if v is None:
        return "Unknown" if value is None else f"Unknown ({value})"
    if v <= 20:
        return "Good"
    if v <= 40:
        return "Fair"
    if v <= 60:
        return "Moderate"
    if v <= 80:
        return "Poor"
    if v <= 100:
        return "Very Poor"
    return "Extremely Poor"


def _aqi_label_us(value: Any) -> str:
    """US AQI band name. Open-Meteo returns a numeric USAQI on the 0–500
    scale: 0–50 Good, 51–100 Moderate, 101–150 Unhealthy for sensitive
    groups, 151–200 Unhealthy, 201–300 Very Unhealthy, 301–500 Hazardous."""
    v = _num(value)
    if v is None:
        return "Unknown" if value is None else f"Unknown ({value})"
    if v <= 50:
        return "Good"
    if v <= 100:
        return "Moderate"
    if v <= 150:
        return "Unhealthy for sensitive groups"
    if v <= 200:
        return "Unhealthy"
    if v <= 300:
        return "Very Unhealthy"
    return "Hazardous"


def _wind_direction(degrees: float | None) -> str:
    """Convert wind degrees to compass direction."""
    if degrees is None:
        return "?"
    directions = ["N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE",
                  "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW"]
    idx = round(degrees / 22.5) % 16
    return directions[idx]


def _num(val: Any) -> float | None:
    """Best-effort numeric coercion. Returns None if missing or non-numeric.

    Avoids ValueError/TypeError when the API returns null or an unexpected type
    and the caller wants a safe numeric comparison.
    """
    if val is None or isinstance(val, bool):
        return None
    try:
        return float(val)
    except (TypeError, ValueError):
        return None


def _format_http_error(e: Exception, api_label: str) -> str:
    """Map an httpx/network exception into an actionable Error: string.

    `api_label` names the failing endpoint family (e.g. "Forecast", "Geocoding")
    so the user/LLM knows where to look.
    """
    if isinstance(e, httpx.TimeoutException):
        return f"Error: {api_label} API request timed out. Retry in a moment."
    if isinstance(e, httpx.HTTPStatusError):
        return f"Error: {api_label} API returned HTTP {e.response.status_code}."
    if isinstance(e, httpx.HTTPError):
        return f"Error: {api_label} API request failed (network error): {e}."
    return f"Error: {api_label} API call failed: {e}"


def _air_quality_advice(eu_aqi: Any, us_aqi: Any) -> str | None:
    """Health-advice string based on the Open-Meteo numeric AQI scales.

    Prefers the European AQI (EAQI 0–100+); falls back to the US AQI (0–500).
    Returns None when no AQI value is available.
    """
    eu_n = _num(eu_aqi)
    us_n = _num(us_aqi)
    if eu_n is not None:
        if eu_n <= 20:
            return "✅ Air quality is good — no health concerns."
        if eu_n <= 40:
            return "✅ Air quality is fair — no health concerns."
        if eu_n <= 60:
            return "⚠️  Moderate air quality. Sensitive individuals should limit prolonged outdoor activity."
        if eu_n <= 80:
            return "⚠️  Poor air quality. Consider reducing outdoor activities, especially if you have respiratory conditions."
        return "🚨 Very poor or extremely poor air quality. Avoid outdoor exertion. Wear a mask if you must go out."
    if us_n is not None:
        if us_n <= 50:
            return "✅ Air quality is good — no health concerns."
        if us_n <= 100:
            return "⚠️  Moderate air quality. Sensitive individuals should limit prolonged outdoor activity."
        if us_n <= 150:
            return "⚠️  Unhealthy for sensitive groups. Reduce prolonged outdoor exertion."
        return "🚨 Unhealthy or hazardous air quality. Avoid outdoor exertion. Wear a mask if you must go out."
    return None


def _geocode(city: str) -> tuple[float, float, str, str] | None:
    """Resolve a city name to coordinates. Returns (lat, lon, name, country).

    Returns None when the city is not found. Raises httpx.HTTPError on a
    network/HTTP failure — the caller is expected to catch and surface it via
    `_format_http_error` so the error is actionable rather than "Internal error".
    """
    params = {"name": city, "count": 3, "language": "en", "format": "json"}
    with httpx.Client(timeout=HTTP_TIMEOUT) as client:
        resp = client.get(GEOCODING_URL, params=params, headers=COMMON_HEADERS)
        resp.raise_for_status()
        data = resp.json()

    results = data.get("results", [])
    if not results:
        return None

    r = results[0]
    return (
        float(r["latitude"]),
        float(r["longitude"]),
        r.get("name", city),
        r.get("country", ""),
    )


# ── Tool implementations ────────────────────────────────────────────────────────

def _weather_status(args: dict[str, Any]) -> str:
    """Self-check: confirm Open-Meteo is reachable and serving data.

    Performs one cheap geocode ("Rome") plus one current-weather probe so we
    exercise the Geocoding + Forecast endpoints in a single round-trip.
    """
    try:
        geo = _geocode("Rome")
        if geo is None:
            return "Error: Geocoding API returned no result for the probe query."
        lat, lon, _, _ = geo

        params = {"latitude": lat, "longitude": lon, "current": "temperature_2m"}
        with httpx.Client(timeout=HTTP_TIMEOUT) as client:
            resp = client.get(FORECAST_URL, params=params, headers=COMMON_HEADERS)
            resp.raise_for_status()
            data = resp.json()

        if not data.get("current"):
            return "Error: Forecast API responded but returned no current data for the probe."

        return (
            "OK: Open-Meteo is reachable. Geocoding and Forecast APIs respond.\n"
            "All tools (get_current_weather, get_forecast, get_air_quality) are operational."
        )
    except Exception as e:
        return _format_http_error(e, "Forecast")


def _weather_current(args: dict[str, Any]) -> str:
    """Get current weather conditions for a city."""
    city = args.get("city", "").strip()
    if not city:
        return "Error: Missing required parameter 'city'."

    units = args.get("units", "metric")
    if units not in ("metric", "imperial"):
        return "Error: 'units' must be 'metric' or 'imperial'."

    try:
        geo = _geocode(city)
        if geo is None:
            return f"Error: Could not find location '{city}'. Check spelling and use English names."
        lat, lon, name, country = geo

        params = {
            "latitude": lat,
            "longitude": lon,
            "current": (
                "temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,"
                "wind_speed_10m,wind_direction_10m,wind_gusts_10m,cloud_cover,"
                "precipitation,rain,uv_index,pressure_msl,visibility"
            ),
            "timezone": "auto",
        }
        if units == "imperial":
            params["temperature_unit"] = "fahrenheit"
            params["wind_speed_unit"] = "mph"
            params["precipitation_unit"] = "inch"

        with httpx.Client(timeout=HTTP_TIMEOUT) as client:
            resp = client.get(FORECAST_URL, params=params, headers=COMMON_HEADERS)
            resp.raise_for_status()
            data = resp.json()
    except Exception as e:
        return _format_http_error(e, "Forecast")

    cur = data.get("current", {})
    if not cur:
        return f"Error: No current weather data available for '{city}'."

    temp_unit  = "°F" if units == "imperial" else "°C"
    wind_unit  = "mph" if units == "imperial" else "km/h"
    precip_unit = "in" if units == "imperial" else "mm"
    # Open-Meteo returns visibility always in meters; no unit selector exists,
    # so we convert explicitly per unit system.
    vis_m = _num(cur.get("visibility"))
    if vis_m is not None:
        if units == "imperial":
            vis_str, vis_unit = f"{vis_m / 1609.34:.1f}", "mi"
        else:
            vis_str, vis_unit = f"{vis_m / 1000:.1f}", "km"
    else:
        vis_str, vis_unit = "?", "km" if units == "metric" else "mi"

    temp       = cur.get("temperature_2m", "?")
    feels_like = cur.get("apparent_temperature", "?")
    humidity   = cur.get("relative_humidity_2m", "?")
    wmo        = cur.get("weather_code")
    wind_speed = cur.get("wind_speed_10m", "?")
    wind_deg   = cur.get("wind_direction_10m")
    wind_gust  = cur.get("wind_gusts_10m")
    cloud      = cur.get("cloud_cover", "?")
    precip     = _num(cur.get("precipitation"))
    rain       = _num(cur.get("rain"))
    uv         = cur.get("uv_index", "?")
    pressure   = cur.get("pressure_msl", "?")

    loc_label = f"{name}, {country}" if country else name
    lines = [
        f"📍 {loc_label} (Current weather)",
        f"",
        f"🌡  Temperature: {temp}{temp_unit}  (feels like {feels_like}{temp_unit})",
        f"☁️  Conditions: {_wmo_desc(wmo)}",
        f"💧  Humidity: {humidity}%",
        f"🌬  Wind: {wind_speed} {wind_unit} from {_wind_direction(wind_deg)}",
    ]

    wind_gust_n = _num(wind_gust)
    if wind_gust_n and wind_gust_n > 0:
        lines.append(f"     Gusts up to {wind_gust_n:g} {wind_unit}")

    lines.append(f"☁️  Cloud cover: {cloud}%")
    lines.append(f"📊  Pressure: {pressure} hPa")
    lines.append(f"👁  Visibility: {vis_str} {vis_unit}")
    lines.append(f"☀️  UV index: {uv}")

    if precip and precip > 0:
        lines.append(f"🌧  Precipitation: {precip:g} {precip_unit}")
    elif rain and rain > 0:
        lines.append(f"🌧  Rain: {rain:g} {precip_unit}")

    return "\n".join(lines)


def _weather_forecast(args: dict[str, Any]) -> str:
    """Get multi-day weather forecast for a city."""
    city = args.get("city", "").strip()
    if not city:
        return "Error: Missing required parameter 'city'."

    days_raw = args.get("days", 5)
    try:
        days = int(days_raw)
    except (TypeError, ValueError):
        return f"Error: 'days' must be an integer between 1 and 16. Got: {days_raw!r}."
    if days < 1 or days > 16:
        return "Error: 'days' must be between 1 and 16."

    units = args.get("units", "metric")
    if units not in ("metric", "imperial"):
        return "Error: 'units' must be 'metric' or 'imperial'."

    try:
        geo = _geocode(city)
        if geo is None:
            return f"Error: Could not find location '{city}'. Check spelling and use English names."
        lat, lon, name, country = geo

        params = {
            "latitude": lat,
            "longitude": lon,
            "daily": (
                "weather_code,temperature_2m_max,temperature_2m_min,"
                "apparent_temperature_max,apparent_temperature_min,"
                "precipitation_sum,rain_sum,precipitation_probability_max,"
                "sunrise,sunset,wind_speed_10m_max,wind_direction_10m_dominant"
            ),
            "forecast_days": days,
            "timezone": "auto",
        }
        if units == "imperial":
            params["temperature_unit"] = "fahrenheit"
            params["wind_speed_unit"] = "mph"
            params["precipitation_unit"] = "inch"

        with httpx.Client(timeout=HTTP_TIMEOUT) as client:
            resp = client.get(FORECAST_URL, params=params, headers=COMMON_HEADERS)
            resp.raise_for_status()
            data = resp.json()
    except Exception as e:
        return _format_http_error(e, "Forecast")

    daily = data.get("daily", {})
    if not daily or "time" not in daily:
        return f"Error: No forecast data available for '{city}'."

    temp_unit   = "°F" if units == "imperial" else "°C"
    wind_unit   = "mph" if units == "imperial" else "km/h"
    precip_unit = "in" if units == "imperial" else "mm"

    loc_label = f"{name}, {country}" if country else name
    lines = [f"📍 {loc_label} — {days}-day forecast"]
    lines.append("")

    times          = daily.get("time", [])
    max_temps      = daily.get("temperature_2m_max", [])
    min_temps      = daily.get("temperature_2m_min", [])
    feel_max       = daily.get("apparent_temperature_max", [])
    feel_min       = daily.get("apparent_temperature_min", [])
    wmos           = daily.get("weather_code", [])
    precip_sum     = daily.get("precipitation_sum", [])
    rain_sum       = daily.get("rain_sum", [])
    precip_prob    = daily.get("precipitation_probability_max", [])
    sunrises       = daily.get("sunrise", [])
    sunsets        = daily.get("sunset", [])
    wind_max       = daily.get("wind_speed_10m_max", [])
    wind_dir       = daily.get("wind_direction_10m_dominant", [])

    for i, t in enumerate(times):
        lines.append(f"── {t} ──")
        lines.append(f"  🌡  {min_temps[i] if i < len(min_temps) else '?'}–{max_temps[i] if i < len(max_temps) else '?'}{temp_unit}"
                     f"  (feels {feel_min[i] if i < len(feel_min) else '?'}–{feel_max[i] if i < len(feel_max) else '?'}{temp_unit})")
        lines.append(f"  ☁️  {_wmo_desc(wmos[i] if i < len(wmos) else None)}")

        prob = precip_prob[i] if i < len(precip_prob) else 0
        ps_n = _num(precip_sum[i] if i < len(precip_sum) else None)
        rs_n = _num(rain_sum[i] if i < len(rain_sum) else None)
        if prob and prob > 0:
            lines.append(f"  🌧  Rain: {prob}% chance"
                         f"{f', {ps_n:g} {precip_unit} precip' if ps_n and ps_n > 0 else ''}"
                         f"{f' ({rs_n:g} {precip_unit} rain)' if rs_n and rs_n > 0 else ''}")

        wd = wind_dir[i] if i < len(wind_dir) else None
        wm_n = _num(wind_max[i] if i < len(wind_max) else None)
        if wm_n is not None and wm_n > 0:
            lines.append(f"  🌬  Wind: up to {wm_n:g} {wind_unit} from {_wind_direction(wd)}")

        sr = sunrises[i] if i < len(sunrises) else ""
        ss = sunsets[i] if i < len(sunsets) else ""
        if sr and ss:
            lines.append(f"  🌅  Sunrise: {sr}  |  🌇 Sunset: {ss}")

        lines.append("")

    return "\n".join(lines)


def _weather_air_quality(args: dict[str, Any]) -> str | dict[str, Any]:
    """Get air quality data for a city.

    On success returns a structured result carrying the formatted text summary
    alongside the raw numeric AQI/pollutant values (see `outputSchema`). On
    failure returns a plain `Error:` string.
    """
    city = args.get("city", "").strip()
    if not city:
        return "Error: Missing required parameter 'city'."

    try:
        geo = _geocode(city)
        if geo is None:
            return f"Error: Could not find location '{city}'. Check spelling and use English names."
        lat, lon, name, country = geo

        params = {
            "latitude": lat,
            "longitude": lon,
            "current": (
                "european_aqi,us_aqi,pm2_5,pm10,"
                "nitrogen_dioxide,ozone,carbon_monoxide,sulphur_dioxide,ammonia"
            ),
            "timezone": "auto",
        }
        with httpx.Client(timeout=HTTP_TIMEOUT) as client:
            resp = client.get(AIR_URL, params=params, headers=COMMON_HEADERS)
            resp.raise_for_status()
            data = resp.json()
    except Exception as e:
        return _format_http_error(e, "Air Quality")

    cur = data.get("current", {})
    if not cur:
        return f"Error: No air quality data available for '{city}'."

    eu_aqi = cur.get("european_aqi")
    us_aqi = cur.get("us_aqi")

    # Numeric pollutant values (None when missing/non-numeric).
    pm25 = _num(cur.get("pm2_5"))
    pm10 = _num(cur.get("pm10"))
    no2  = _num(cur.get("nitrogen_dioxide"))
    o3   = _num(cur.get("ozone"))
    co   = _num(cur.get("carbon_monoxide"))
    so2  = _num(cur.get("sulphur_dioxide"))
    nh3  = _num(cur.get("ammonia"))
    pollutants = {"pm2_5": pm25, "pm10": pm10, "no2": no2, "o3": o3,
                  "co": co, "so2": so2, "nh3": nh3}

    eu_label = _aqi_label_eu(eu_aqi) if eu_aqi is not None else None
    us_label = _aqi_label_us(us_aqi) if us_aqi is not None else None
    advice = _air_quality_advice(eu_aqi, us_aqi)

    loc_label = f"{name}, {country}" if country else name

    # ── Formatted text summary (kept inside the structured payload so the LLM
    #    still has the human-readable emoji output alongside the raw numbers).
    def _fmt(val: float | None) -> str:
        return f"{val:g} µg/m³" if val is not None else "?"

    lines = [f"📍 {loc_label} (Air Quality)", ""]
    if eu_aqi is not None:
        lines.append(f"🇪🇺 European AQI: {eu_aqi} ({eu_label})")
    if us_aqi is not None:
        lines.append(f"🇺🇸 US AQI: {us_aqi} ({us_label})")
    lines.append("")
    lines.append("  • PM2.5: " + _fmt(pm25))
    lines.append("  • PM10:  " + _fmt(pm10))
    lines.append("  • NO₂:   " + _fmt(no2))
    lines.append("  • O₃:    " + _fmt(o3))
    lines.append("  • CO:    " + _fmt(co))
    lines.append("  • SO₂:   " + _fmt(so2))
    lines.append("  • NH₃:   " + _fmt(nh3))
    lines.append("")
    if advice:
        lines.append(advice)

    return {
        "location": loc_label,
        "summary": "\n".join(lines),
        "european_aqi": eu_aqi,
        "european_aqi_label": eu_label,
        "us_aqi": us_aqi,
        "us_aqi_label": us_label,
        "pollutants_ug_m3": pollutants,
        "health_advice": advice,
    }


# ── Tool manifest ────────────────────────────────────────────────────────────────

TOOLS = [
    {
        "name": "status",
        "description": (
            "Self-check that the Weather integration is operational: verifies the "
            "Open-Meteo Geocoding and Forecast APIs are reachable by performing one "
            "cheap probe (geocode 'Rome' + current temperature). Call this first "
            "whenever another weather tool fails, or to give the user a quick yes/no "
            "on whether weather data is usable right now."
        ),
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "get_current_weather",
        "description": (
            "Get current weather conditions for any city worldwide. "
            "Returns temperature, feels-like, humidity, wind (speed/direction/gusts), "
            "conditions (clear/rain/snow/etc.), cloud cover, pressure, visibility, "
            "UV index, and precipitation. Free, no API key required."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "City name in English (e.g. 'London', 'Rome', 'Tokyo').",
                },
                "units": {
                    "type": "string",
                    "enum": ["metric", "imperial"],
                    "description": "Unit system. 'metric' = °C, km/h, mm; 'imperial' = °F, mph, in. Default: 'metric'.",
                },
            },
            "required": ["city"],
        },
    },
    {
        "name": "get_forecast",
        "description": (
            "Get multi-day weather forecast for any city worldwide. "
            "Returns daily min/max temperature (and feels-like), conditions, rain probability, "
            "precipitation amounts, wind max/direction, sunrise/sunset times. "
            "Use when you need to plan upcoming days. Free, no API key required."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "City name in English (e.g. 'Paris', 'New York').",
                },
                "days": {
                    "type": "integer",
                    "description": "Number of forecast days (1–16). Default: 5.",
                },
                "units": {
                    "type": "string",
                    "enum": ["metric", "imperial"],
                    "description": "Unit system. 'metric' = °C, km/h, mm; 'imperial' = °F, mph, in. Default: 'metric'.",
                },
            },
            "required": ["city"],
        },
    },
    {
        "name": "get_air_quality",
        "description": (
            "Get current air quality for any city worldwide. "
            "Returns European and US AQI indices, plus detailed pollutant levels: "
            "PM2.5, PM10, NO₂, O₃, CO, SO₂, NH₃. Includes health advice based on the AQI level. "
            "Returns structured content: a `summary` text plus the raw numeric AQI and "
            "pollutant values for machine consumption. Free, no API key required."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "City name in English (e.g. 'Beijing', 'London').",
                },
            },
            "required": ["city"],
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "location": {"type": "string"},
                "summary": {"type": "string"},
                "european_aqi": {"type": ["number", "null"]},
                "european_aqi_label": {"type": ["string", "null"]},
                "us_aqi": {"type": ["number", "null"]},
                "us_aqi_label": {"type": ["string", "null"]},
                "pollutants_ug_m3": {
                    "type": "object",
                    "properties": {
                        "pm2_5": {"type": ["number", "null"]},
                        "pm10":  {"type": ["number", "null"]},
                        "no2":   {"type": ["number", "null"]},
                        "o3":    {"type": ["number", "null"]},
                        "co":    {"type": ["number", "null"]},
                        "so2":   {"type": ["number", "null"]},
                        "nh3":   {"type": ["number", "null"]},
                    },
                },
                "health_advice": {"type": ["string", "null"]},
            },
        },
    },
]

TOOL_DISPATCH = {
    "status":             _weather_status,
    "get_current_weather": _weather_current,
    "get_forecast":        _weather_forecast,
    "get_air_quality":     _weather_air_quality,
}


# ── JSON-RPC dispatch ────────────────────────────────────────────────────────────

def _ok(req_id: Any, result: Any) -> str:
    return json.dumps({"jsonrpc": "2.0", "id": req_id, "result": result})


def _text_result(req_id: Any, text: str, is_error: bool = False) -> str:
    payload: dict = {
        "jsonrpc": "2.0",
        "id": req_id,
        "result": {"content": [{"type": "text", "text": text}]},
    }
    if is_error:
        payload["result"]["isError"] = True
    return json.dumps(payload)


def _structured_result(req_id: Any, structured: dict) -> str:
    """Build a JSON-RPC result carrying structuredContent (canonical for MCP
    structured tool results) plus a text mirror in `content[]` for plain
    clients. The structured object is expected to embed a human-readable
    `summary` string alongside the raw numeric fields.

    Skald prefers structuredContent when present, so the LLM sees the JSON
    object (which contains the formatted summary)."""
    summary = structured.get("summary")
    if not isinstance(summary, str):
        summary = json.dumps(structured, ensure_ascii=False)
    return json.dumps({
        "jsonrpc": "2.0",
        "id": req_id,
        "result": {
            "content": [{"type": "text", "text": summary}],
            "structuredContent": structured,
        },
    })


def _error(req_id: Any, code: int, message: str) -> str:
    return json.dumps({
        "jsonrpc": "2.0",
        "id": req_id,
        "error": {"code": code, "message": message},
    })


def handle_request(msg: dict) -> str | None:
    method = msg.get("method", "")
    req_id = msg.get("id")

    if method == "initialize":
        return _ok(req_id, {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {
                "name": "weather",
                "version": "1.2.0",
            },
        })

    if method == "notifications/initialized":
        return None

    if method == "ping":
        return _ok(req_id, {})

    if method == "tools/list":
        return _ok(req_id, {"tools": TOOLS})

    if method == "tools/call":
        params = msg.get("params", {})
        tool_name = params.get("name", "")
        tool_args = params.get("arguments", {})

        handler = TOOL_DISPATCH.get(tool_name)
        if handler is None:
            return _text_result(req_id, f"Error: Unknown tool: {tool_name}", is_error=True)

        try:
            result = handler(tool_args)
            # A dict return is a structured result (structuredContent); a str
            # return is plain text (an "Error:" prefix marks it as isError).
            if isinstance(result, dict):
                return _structured_result(req_id, result)
            is_err = result.startswith("Error:")
            return _text_result(req_id, result, is_error=is_err)
        except Exception as e:
            log(f"Unhandled exception in tool '{tool_name}': {e}")
            return _text_result(req_id, f"Error: Internal error in tool '{tool_name}': {e}", is_error=True)

    return _error(req_id, -32601, f"Method not found: {method}")


# ── Main loop ────────────────────────────────────────────────────────────────────

def main() -> None:
    log("Starting weather MCP server (Open-Meteo)")
    try:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError as e:
                log(f"Invalid JSON input: {e}")
                continue

            resp = handle_request(msg)
            if resp is not None:
                sys.stdout.write(resp + "\n")
                sys.stdout.flush()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
