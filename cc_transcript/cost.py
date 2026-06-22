from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Mapping

    from cc_transcript.models import AssistantEvent, Usage

MTOK = 1_000_000


@dataclass(frozen=True, slots=True)
class ModelPricing:
    """USD-per-million-token rates for a model family.

    Standard service tier, standard (non-long-context) pricing — current Claude
    4.x models serve their 1M context at standard rates with no long-context
    premium. Override via the `pricing` argument to cost_of for other tiers.

    Attributes:
        input: USD per million uncached input tokens.
        output: USD per million output tokens.
        cache_read: USD per million tokens served from the cache.
        cache_write_5m: USD per million tokens written to the 5-minute cache.
        cache_write_1h: USD per million tokens written to the 1-hour cache.
    """

    input: float
    output: float
    cache_read: float
    cache_write_5m: float
    cache_write_1h: float


@dataclass(frozen=True, slots=True)
class CostBreakdown:
    """The per-component and total USD cost of a single turn's token usage.

    Attributes:
        input_cost: USD cost of the uncached input tokens.
        output_cost: USD cost of the output tokens.
        cache_read_cost: USD cost of the cache-read tokens.
        cache_write_cost: USD cost of the cache-creation tokens, summed across TTL buckets.
        total: The sum of the four component costs.
    """

    input_cost: float
    output_cost: float
    cache_read_cost: float
    cache_write_cost: float
    total: float


PRICING: Mapping[str, ModelPricing] = {
    "fable": ModelPricing(input=10.0, output=50.0, cache_read=1.0, cache_write_5m=12.5, cache_write_1h=20.0),
    "opus": ModelPricing(input=5.0, output=25.0, cache_read=0.5, cache_write_5m=6.25, cache_write_1h=10.0),
    "sonnet": ModelPricing(input=3.0, output=15.0, cache_read=0.3, cache_write_5m=3.75, cache_write_1h=6.0),
    "haiku": ModelPricing(input=1.0, output=5.0, cache_read=0.1, cache_write_5m=1.25, cache_write_1h=2.0),
}


def resolve_pricing(model: str, *, pricing: Mapping[str, ModelPricing] = PRICING) -> ModelPricing:
    """Return the pricing row whose family key is a substring of `model`.

    Family keys are mutually exclusive substrings, so at most one matches —
    ``"claude-haiku-4-5-20251001"`` resolves to haiku, ``"claude-opus-4-8"`` to
    opus, and a bare ``"sonnet"`` to sonnet.

    Args:
        model: The model id to resolve.
        pricing: The family-keyed pricing table to search.

    Raises:
        KeyError: If no family key is a substring of `model`.

    Example:
        >>> resolve_pricing("claude-opus-4-8").input
        5.0
    """
    for family, row in pricing.items():
        if family in model:
            return row
    raise KeyError(f"no pricing for model: {model}")


def cost_of(usage: Usage, model: str, *, pricing: Mapping[str, ModelPricing] = PRICING) -> CostBreakdown:
    """Compute the USD cost of a turn's token usage under a model's rates.

    The cache-creation split prefers the per-TTL ``usage.cache_creation`` when
    present, falling back to the flat ``usage.cache_creation_input_tokens`` as
    5-minute writes with no 1-hour share.

    Args:
        usage: The turn's token usage and cache accounting.
        model: The model id whose rates apply.
        pricing: The family-keyed pricing table to resolve against.

    Returns:
        The per-component and total cost.

    Example:
        >>> cost_of(usage, "claude-haiku-4-5-20251001").total
        0.05759
    """
    row = resolve_pricing(model, pricing=pricing)
    write_5m, write_1h = (
        (usage.cache_creation.ephemeral_5m_input_tokens, usage.cache_creation.ephemeral_1h_input_tokens)
        if usage.cache_creation is not None
        else (usage.cache_creation_input_tokens, 0)
    )
    input_cost = usage.input_tokens / MTOK * row.input
    output_cost = usage.output_tokens / MTOK * row.output
    cache_read_cost = usage.cache_read_input_tokens / MTOK * row.cache_read
    cache_write_cost = write_5m / MTOK * row.cache_write_5m + write_1h / MTOK * row.cache_write_1h
    return CostBreakdown(
        input_cost=input_cost,
        output_cost=output_cost,
        cache_read_cost=cache_read_cost,
        cache_write_cost=cache_write_cost,
        total=input_cost + output_cost + cache_read_cost + cache_write_cost,
    )


def cost_of_assistant(event: AssistantEvent, *, pricing: Mapping[str, ModelPricing] = PRICING) -> CostBreakdown | None:
    """Compute the cost of an assistant turn, or None when it carries no usage.

    Args:
        event: The assistant turn whose usage is priced.
        pricing: The family-keyed pricing table to resolve against.

    Returns:
        The cost breakdown, or None when ``event.usage`` is None.
    """
    if event.usage is None:
        return None
    return cost_of(event.usage, event.model, pricing=pricing)
