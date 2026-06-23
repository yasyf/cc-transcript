from __future__ import annotations

import pytest

from cc_transcript.cost import MTOK, cost_of, resolve_pricing
from cc_transcript.models import CacheCreation, ServerToolUse, Usage


def usage(
    *,
    input_tokens: int = 0,
    output_tokens: int = 0,
    cache_read_input_tokens: int = 0,
    cache_creation_input_tokens: int = 0,
    cache_creation: CacheCreation | None = None,
) -> Usage:
    return Usage(
        input_tokens=input_tokens,
        output_tokens=output_tokens,
        cache_read_input_tokens=cache_read_input_tokens,
        cache_creation_input_tokens=cache_creation_input_tokens,
        cache_creation=cache_creation,
        service_tier="standard",
        inference_geo="not_available",
        server_tool_use=ServerToolUse(web_search_requests=0, web_fetch_requests=0),
    )


@pytest.mark.parametrize(
    ("model", "input_cost", "output_cost", "cache_read_cost", "cache_write_cost", "total"),
    [
        pytest.param("claude-fable-5", 10.0, 50.0, 1.0, 32.5, 93.5, id="fable"),
        pytest.param("claude-opus-4-8", 5.0, 25.0, 0.5, 16.25, 46.75, id="opus"),
        pytest.param("claude-sonnet-4-6", 3.0, 15.0, 0.3, 9.75, 28.05, id="sonnet"),
        pytest.param("claude-haiku-4-5", 1.0, 5.0, 0.1, 3.25, 9.35, id="haiku"),
    ],
)
def test_rate_math_per_family(
    model: str,
    input_cost: float,
    output_cost: float,
    cache_read_cost: float,
    cache_write_cost: float,
    total: float,
) -> None:
    breakdown = cost_of(
        usage(
            input_tokens=MTOK,
            output_tokens=MTOK,
            cache_read_input_tokens=MTOK,
            cache_creation=CacheCreation(ephemeral_5m_input_tokens=MTOK, ephemeral_1h_input_tokens=MTOK),
        ),
        model,
    )
    assert breakdown.input_cost == input_cost
    assert breakdown.output_cost == output_cost
    assert breakdown.cache_read_cost == cache_read_cost
    assert breakdown.cache_write_cost == cache_write_cost
    assert breakdown.total == total


def test_flat_fallback_charges_5m_rate() -> None:
    breakdown = cost_of(usage(cache_creation_input_tokens=MTOK, cache_creation=None), "claude-haiku-4-5")
    assert breakdown.cache_write_cost == 1.25
    assert breakdown.total == 1.25


def test_haiku_fixture_reconciles_to_the_cent() -> None:
    fixture_usage = Usage(
        input_tokens=28,
        output_tokens=250,
        cache_read_input_tokens=51020,
        cache_creation_input_tokens=25605,
        cache_creation=CacheCreation(ephemeral_5m_input_tokens=0, ephemeral_1h_input_tokens=25605),
        service_tier="standard",
        inference_geo="not_available",
        server_tool_use=ServerToolUse(web_search_requests=0, web_fetch_requests=0),
    )
    assert abs(cost_of(fixture_usage, "claude-haiku-4-5-20251001").total - 0.05759) < 1e-9


def test_resolve_pricing_matches_opus() -> None:
    row = resolve_pricing("claude-opus-4-8")
    assert (row.input, row.output, row.cache_read, row.cache_write_5m, row.cache_write_1h) == (
        5.0,
        25.0,
        0.5,
        6.25,
        10.0,
    )


def test_resolve_pricing_unknown_model_raises() -> None:
    with pytest.raises(KeyError, match="no pricing for model: gpt-4"):
        resolve_pricing("gpt-4")
