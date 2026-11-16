#!/usr/bin/env python3
"""Regenerate ZP's built-in model catalog from pi-ai's catalog data.

Usage:
    scripts/update_catalog.py <path to @earendil-works/pi-ai/dist/providers/data>

Only chat models on the protocols ZP implements are kept, and only the fields ZP reads.
The output lands in crates/zp-ai/catalog/<provider>.json.
"""

import json
import sys
from pathlib import Path

PROVIDERS = [
    "anthropic",
    "openai",
    "openai-codex",
    "github-copilot",
    "openrouter",
    "deepseek",
    "moonshotai",
    "moonshotai-cn",
    "zai",
    "zai-coding-cn",
    "groq",
    "kimi-coding",
    "xai",
]
SUPPORTED_APIS = {
    "anthropic-messages",
    "openai-completions",
    "openai-responses",
    "openai-codex-responses",
}
MODEL_FIELDS = [
    "id",
    "name",
    "api",
    "provider",
    "baseUrl",
    "reasoning",
    "input",
    "cost",
    "contextWindow",
    "maxTokens",
    "thinkingLevelMap",
    "headers",
    "compat",
]
COMPAT_FIELDS = {
    # anthropic-messages
    "forceAdaptiveThinking",
    "supportsTemperature",
    "supportsEagerToolInputStreaming",
    "allowEmptySignature",
    # openai-completions
    "supportsStore",
    "supportsDeveloperRole",
    "supportsReasoningEffort",
    "supportsUsageInStreaming",
    "maxTokensField",
    "requiresToolResultName",
    "requiresAssistantAfterToolResult",
    "requiresThinkingAsText",
    "requiresReasoningContentOnAssistantMessages",
    "thinkingFormat",
    "supportsStrictMode",
    "zaiToolStream",
    "cacheControlFormat",
}


def convert(model: dict) -> dict:
    out = {key: model[key] for key in MODEL_FIELDS if key in model}
    compat = {k: v for k, v in (model.get("compat") or {}).items() if k in COMPAT_FIELDS}
    if compat:
        out["compat"] = compat
    else:
        out.pop("compat", None)
    return out


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    source = Path(sys.argv[1])
    target = Path(__file__).resolve().parent.parent / "crates" / "zp-ai" / "catalog"
    target.mkdir(parents=True, exist_ok=True)
    for provider in PROVIDERS:
        data = json.loads((source / f"{provider}.json").read_text())
        models = [
            convert(model)
            for api, entries in data.items()
            if api in SUPPORTED_APIS
            for model in entries.values()
            if model.get("type", "chat") == "chat"
        ]
        models.sort(key=lambda m: m["id"])
        path = target / f"{provider}.json"
        path.write_text(json.dumps(models, indent=1, ensure_ascii=False) + "\n")
        print(f"{provider:16} {len(models):4} models -> {path.relative_to(target.parent.parent.parent)}")


if __name__ == "__main__":
    main()
