#!/usr/bin/env python3
"""
MCP Server for Google Trends data via trendspyg.

Provides tools to query Google Trends: interest over time, related queries,
interest by region, trending now (RSS), and bulk trending CSVs.

Uses trendspyg v0.7.0 as the data backend.
Browser-based tools require Chrome installed on the host.
RSS-based tools require no browser and return in ~0.2s.

Rate limits: Google Trends is a public service. Browser-based queries should
be spaced 5-10 seconds apart to avoid HTTP 429. RSS is lighter but still
subject to rate limiting on excessive polling.

Transport: stdio JSON-RPC (mcp.run() default). All diagnostics go to stderr —
never stdout — to avoid corrupting the protocol stream.

Output: tools return plain dicts, so FastMCP emits real ``structuredContent``
(a JSON object) plus a pretty-printed text fallback. Errors are raised as
``ToolError`` → the client receives an ``isError`` result carrying an
LLM-actionable hint.
"""

import functools
import json
import sys
import traceback
from typing import Annotated, Any, Literal

import anyio
from mcp.server.fastmcp import FastMCP
from mcp.server.fastmcp.exceptions import ToolError
from pydantic import Field

# ── trendspyg imports ─────────────────────────────────────────────────────────
from trendspyg.explore import (
    download_google_trends_explore,
    download_google_trends_interest_over_time,
)
from trendspyg.downloader import download_google_trends_csv, CATEGORIES, COUNTRIES
from trendspyg.rss_downloader import download_google_trends_rss

# ── Server init ───────────────────────────────────────────────────────────────
mcp = FastMCP("google_trends_mcp")

# ── Typed parameter aliases (drive JSON-schema validation) ────────────────────
Hours = Literal[4, 24, 48, 168]
SortBy = Literal["relevance", "title", "volume", "recency"]
CsvCategory = Literal[
    "all", "autos", "beauty", "business", "climate", "entertainment", "food",
    "games", "health", "hobbies", "lifestyle", "media", "pets", "science",
    "shopping", "sports", "stories", "technology", "travel",
]

Json = dict[str, Any]


# ── Utility functions ─────────────────────────────────────────────────────────

def _clean(obj: Any) -> Any:
    """Recursively convert data into plain JSON-safe Python types.

    Handles datetimes (→ ISO string), numpy scalars (→ native via .item()),
    and nested dicts/lists/tuples. Guarantees the result is serializable by
    FastMCP (both for ``structuredContent`` and the text fallback).
    """
    if isinstance(obj, dict):
        return {k: _clean(v) for k, v in obj.items()}
    if isinstance(obj, (list, tuple)):
        return [_clean(v) for v in obj]
    if hasattr(obj, "isoformat"):  # datetime / date
        return obj.isoformat()
    if hasattr(obj, "item") and not isinstance(obj, (str, bytes)):  # numpy scalar
        try:
            return obj.item()
        except Exception:
            return obj
    return obj


def _envelope(data: Any) -> Json:
    """Normalize trendspyg output to a JSON object for structured tool output.

    trendspyg returns a dict for every mode we call; if a future version hands
    back a bare list we wrap it so ``structuredContent`` stays a JSON object.
    """
    cleaned = _clean(data)
    return cleaned if isinstance(cleaned, dict) else {"items": cleaned}


def _json(data: Any) -> str:
    """Serialize to a JSON string — used for MCP resources (content, not tools)."""
    return json.dumps(_clean(data), indent=2, ensure_ascii=False, default=str)


async def _to_thread(fn, **kwargs) -> Any:
    """Run a blocking trendspyg call off the event loop so the server stays
    responsive during multi-second browser sessions."""
    return await anyio.to_thread.run_sync(functools.partial(fn, **kwargs))


def _tool_error(e: Exception, tool: str, subject: str) -> ToolError:
    """Build a consistent, LLM-actionable ToolError; log the full traceback to
    stderr (safe for stdio transport)."""
    traceback.print_exc(file=sys.stderr)
    msg = str(e).lower()

    if ("rate" in msg and "limit" in msg) or "429" in msg or "too many" in msg:
        hint = (
            f"Rate limited by Google Trends while querying '{subject}'. "
            f"Wait 30-60 seconds before retrying. "
            f"Tip: google_trends_rss has a lighter rate-limit footprint."
        )
    elif any(k in msg for k in ("chromedriver", "selenium", "webdriver", "session not created")):
        hint = (
            f"Browser required for '{tool}' but Chrome/WebDriver is unavailable on this host. "
            f"Use google_trends_rss instead — it works over plain HTTP, no browser."
        )
    elif "chrome" in msg or "binary" in msg:
        hint = (
            f"Chrome browser not found — '{tool}' requires Chrome installed. "
            f"Install Chrome, or use google_trends_rss for browser-free trend data."
        )
    elif "not found" in msg or "404" in msg or "no data" in msg:
        hint = f"No data found for '{subject}'. Try a different keyword or a broader timeframe."
    elif "invalid" in msg or "unsupported" in msg:
        hint = f"Invalid parameter for '{tool}': {e}. Use google_trends_countries for valid geo codes."
    else:
        hint = f"Error in {tool} for '{subject}': {type(e).__name__}: {e}"

    return ToolError(hint)


# ── Tools ─────────────────────────────────────────────────────────────────────

@mcp.tool(
    name="google_trends_interest_over_time",
    annotations={
        "title": "Google Trends — Interest Over Time",
        "readOnlyHint": True,
        "destructiveHint": False,
        "idempotentHint": True,
        "openWorldHint": True,
    },
)
async def google_trends_interest_over_time(
    keyword: Annotated[str, Field(
        description="Search term (e.g. 'bitcoin', 'running shoes', 'AI').",
        min_length=1, max_length=200,
    )],
    geo: Annotated[str, Field(
        description="ISO country code (e.g. 'US', 'GB', 'IT') or 'US-CA' for US states. Empty = worldwide.",
    )] = "",
    timeframe: Annotated[str, Field(
        description="Time window. Examples: 'today 12-m', 'today 5-y', 'today 3-m', "
                    "'today 1-m', '2023-01-01 2023-12-31', 'now 7-d', 'now 1-H'.",
    )] = "today 12-m",
    category: Annotated[int, Field(
        description="Google Trends category ID (0 = all). See google_trends_categories.",
        ge=0,
    )] = 0,
) -> Json:
    """Get search interest over time for a keyword.

    Returns a time series of relative popularity (0-100 scale) for a search term.
    Requires Chrome browser installed on the host (headless mode).

    Each data point has:
    - date: ISO date string
    - value: relative search interest (0-100, normalized within the query)
    - is_partial: true if the current period's data is still incomplete

    Use when: tracking keyword popularity trends, comparing seasonal patterns,
    validating market timing for a product/idea.

    Returns:
        dict: structured payload with an interest_over_time array.

    Examples:
        - "Interest in 'electric cars' over the last year in the UK?"
          → keyword="electric cars", geo="GB", timeframe="today 12-m"
        - "Bitcoin search trend in Italy last 90 days"
          → keyword="bitcoin", geo="IT", timeframe="today 3-m"
    """
    try:
        data = await _to_thread(
            download_google_trends_interest_over_time,
            keyword=keyword,
            geo=geo,
            timeframe=timeframe,
            category=category,
            headless=True,
            output_format="dict",
        )
        # trendspyg returns a bare list of points here; wrap it with the query
        # context so the structured payload is self-describing for the LLM.
        return {
            "keyword": keyword,
            "geo": geo or "worldwide",
            "timeframe": timeframe,
            "category": category,
            "interest_over_time": _clean(data),
        }
    except Exception as e:
        raise _tool_error(e, "interest_over_time", keyword)


@mcp.tool(
    name="google_trends_explore",
    annotations={
        "title": "Google Trends — Full Explore",
        "readOnlyHint": True,
        "destructiveHint": False,
        "idempotentHint": True,
        "openWorldHint": True,
    },
)
async def google_trends_explore(
    keyword: Annotated[str, Field(
        description="Search term (e.g. 'bitcoin', 'running shoes').",
        min_length=1, max_length=200,
    )],
    geo: Annotated[str, Field(
        description="ISO country code (e.g. 'US', 'GB', 'IT') or 'US-CA' for US states. Empty = worldwide.",
    )] = "",
    timeframe: Annotated[str, Field(
        description="Time window (same format as interest_over_time).",
    )] = "today 12-m",
    category: Annotated[int, Field(
        description="Google Trends category ID (0 = all). See google_trends_categories.",
        ge=0,
    )] = 0,
    include_related: Annotated[bool, Field(
        description="Include related queries (top + rising). Adds ~2-3s.",
    )] = True,
    include_geo: Annotated[bool, Field(
        description="Include interest-by-region breakdown. Adds ~1-2s.",
    )] = True,
) -> Json:
    """Full Google Trends Explore: interest over time + related queries + interest by region.

    The most comprehensive tool — fetches all available data for a keyword in a
    single browser session. Returns:
    - interest_over_time: array of {date, value, is_partial}
    - related_queries: {top: [{query, value, link}], rising: [{query, value, link}]}
    - interest_by_region: [{geo_code, geo_name, value}]

    Requires Chrome browser installed on the host.

    Use when: you need the complete picture — trend direction, what people also
    search, and where interest is concentrated geographically.

    Returns:
        dict: structured payload with all three data sections.

    Examples:
        - "Full Trends picture for 'vegan protein' in the US?"
          → keyword="vegan protein", geo="US"
        - "Quick check on 'climate change' trend"
          → keyword="climate change", timeframe="today 5-y", include_related=False
    """
    try:
        data = await _to_thread(
            download_google_trends_explore,
            keyword=keyword,
            geo=geo,
            timeframe=timeframe,
            category=category,
            headless=True,
            include_related=include_related,
            include_geo=include_geo,
        )
        return _envelope(data)
    except Exception as e:
        raise _tool_error(e, "explore", keyword)


@mcp.tool(
    name="google_trends_rss",
    annotations={
        "title": "Google Trends — Trending Now (RSS)",
        "readOnlyHint": True,
        "destructiveHint": False,
        "idempotentHint": False,
        "openWorldHint": True,
    },
)
async def google_trends_rss(
    geo: Annotated[str, Field(
        description="ISO country code (e.g. 'US', 'GB', 'IT').",
    )] = "US",
    include_images: Annotated[bool, Field(
        description="Include trend images. Adds data volume.",
    )] = False,
    include_articles: Annotated[bool, Field(
        description="Include news articles for each trend. Adds data volume.",
    )] = False,
) -> Json:
    """Get currently trending searches via the Google Trends RSS feed.

    ⚡ Fast path: pure HTTP, no browser needed, returns in ~0.2s.

    Returns up to ~20 trending topics with optional images and news articles.
    Each trend includes:
    - keyword: topic name
    - volume_text / volume_min: estimated search-volume indicator (e.g. "500+")
    - explore_url: deep link to the Google Trends Explore page
    - started_at / ended_at / is_active: trend lifecycle timestamps
    - image (optional): representative image URL
    - news (optional): up to 5 related news articles

    Use when: you want to know what's trending *right now* — real-time
    monitoring, content ideation, newsjacking.

    Returns:
        dict: structured payload with a trends array.

    Examples:
        - "What's trending in the UK right now?" → geo="GB"
        - "US trends with news context" → geo="US", include_articles=True
    """
    try:
        data = await _to_thread(
            download_google_trends_rss,
            geo=geo,
            output_format="dict",
            include_images=include_images,
            include_articles=include_articles,
            max_articles_per_trend=5,
            cache=False,
            normalize=True,
        )
        return _envelope(data)
    except Exception as e:
        raise _tool_error(e, "rss", geo)


@mcp.tool(
    name="google_trends_csv",
    annotations={
        "title": "Google Trends — Trending CSV (Bulk)",
        "readOnlyHint": True,
        "destructiveHint": False,
        "idempotentHint": False,
        "openWorldHint": True,
    },
)
async def google_trends_csv(
    geo: Annotated[str, Field(
        description="ISO country code (e.g. 'US', 'GB', 'IT').",
    )] = "US",
    hours: Annotated[Hours, Field(
        description="Lookback window in hours. One of: 4, 24, 48, 168 (7d).",
    )] = 24,
    category: Annotated[CsvCategory, Field(
        description="Trend category (e.g. 'all', 'technology', 'business', 'sports').",
    )] = "all",
    sort_by: Annotated[SortBy, Field(
        description="Sort order: 'relevance', 'title', 'volume', 'recency'.",
    )] = "relevance",
) -> Json:
    """Download bulk trending searches via Google Trends CSV export.

    Returns up to ~480 current trending topics, filterable by time window,
    category, and sort order. Requires Chrome browser (headless mode).

    Each trend includes: trend name, traffic estimate, explore link, and
    published timestamp.

    Use when: you need a large dataset of current trends for market research,
    category analysis, or trend scouting across niches.

    Returns:
        dict: structured payload with the trends collection.

    Examples:
        - "All trending tech topics in the US in the last 24h"
          → geo="US", hours=24, category="technology"
        - "Trending UK business this past week"
          → geo="GB", hours=168, category="business", sort_by="volume"
    """
    try:
        data = await _to_thread(
            download_google_trends_csv,
            geo=geo,
            hours=hours,
            category=category,
            sort_by=sort_by,
            headless=True,
            normalize=True,  # returns a unified envelope dict (ignores output_format)
            timeout=15,
        )
        return _envelope(data)
    except Exception as e:
        raise _tool_error(e, "csv", f"{geo}/{category}")


@mcp.tool(
    name="google_trends_categories",
    annotations={
        "title": "Google Trends — Available Categories",
        "readOnlyHint": True,
        "destructiveHint": False,
        "idempotentHint": True,
        "openWorldHint": False,
    },
)
def google_trends_categories() -> Json:
    """List all Google Trends categories available for filtering.

    Use this to discover valid category names before calling google_trends_csv
    with a specific category filter.

    Returns:
        dict: category names → labels,
              e.g. {"all": "All categories", "technology": "Technology", ...}
    """
    return dict(CATEGORIES)


@mcp.tool(
    name="google_trends_countries",
    annotations={
        "title": "Google Trends — Available Countries & Regions",
        "readOnlyHint": True,
        "destructiveHint": False,
        "idempotentHint": True,
        "openWorldHint": False,
    },
)
def google_trends_countries() -> Json:
    """List all ISO country codes and US state codes accepted by geo parameters.

    Returns:
        dict: 'countries' (ISO codes → names) and 'us_states' (US-XX → names).
    """
    from trendspyg.downloader import US_STATES
    return {
        "note": "Use ISO codes (e.g. 'US', 'GB', 'IT') for geo params. Empty string = worldwide.",
        "countries": dict(COUNTRIES),
        "us_states": dict(US_STATES),
    }


# ── Resources ─────────────────────────────────────────────────────────────────

@mcp.resource("trends://categories")
def trends_categories() -> str:
    """Available Google Trends categories as a resource."""
    return _json(CATEGORIES)


@mcp.resource("trends://countries")
def trends_countries() -> str:
    """Available countries and US states as a resource."""
    from trendspyg.downloader import US_STATES
    return _json({"countries": COUNTRIES, "us_states": US_STATES})


# ── Entry point ───────────────────────────────────────────────────────────────

if __name__ == "__main__":
    mcp.run()
