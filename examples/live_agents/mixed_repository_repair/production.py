from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
from typing import Any, Literal, cast

import troupe


PROFILE_ENVS = {
    "codex": "TROUPE_LIVE_CODEX_PROFILE",
    "claude": "TROUPE_LIVE_CLAUDE_PROFILE",
    "kimi": "TROUPE_LIVE_KIMI_PROFILE",
}


class PayloadEffect(troupe.Effect):
    def __init__(self, payload: dict[str, Any]) -> None:
        self.payload = payload


class Investigation(PayloadEffect):
    pass


class ContractReview(PayloadEffect):
    pass


class RepositoryRepair(PayloadEffect):
    pass


class ContextRecall(PayloadEffect):
    pass


class PayloadActor(troupe.Actor):
    def emit(
        self,
        effect_type: type[PayloadEffect],
        payload: dict[str, Any],
    ) -> tuple[troupe.Effect, ...]:
        return (
            self.make_effect(
                effect_type,
                effect_args=(payload,),
                effect_kwargs={},
            ),
        )


class CodexInvestigator(PayloadActor):
    def __init__(self, repository: Path) -> None:
        self.repository = repository

    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        operation = cue.instruction["operation"]
        if operation == "investigate":
            result = await self.act(
                script=(
                    "ROLE: investigator. Use repository tools to inspect ISSUE.md, "
                    "repair.py, and test_repair.py. Do not modify any file. Diagnose the "
                    "defect and preserve the investigation-id in your session context. "
                    "Return the structured investigation through the Troupe result tool."
                ),
                output_schema={
                    "role": troupe.act_schema.StrValue(
                        description="the Actor's repository role",
                        choices=["investigator"],
                    ),
                    "investigation_id": troupe.act_schema.StrValue(
                        description="the exact investigation-id from ISSUE.md",
                        min_length=1,
                    ),
                    "target_file": troupe.act_schema.StrValue(
                        description="the defective implementation file",
                        choices=["repair.py"],
                    ),
                    "root_cause": troupe.act_schema.StrValue(
                        description="the observed implementation defect",
                        choices=["normalize_title returns its input unchanged"],
                    ),
                    "expected_behavior": troupe.act_schema.StrValue(
                        description="the behavior required by the issue and test",
                        choices=[
                            "strip surrounding whitespace and title-case each word"
                        ],
                    ),
                },
            )
            (self.repository / "ISSUE.md").unlink()
            return self.emit(Investigation, result)

        if operation == "recall":
            result = await self.act(
                script=(
                    "ROLE: investigator recall. Do not read files, run commands, or use "
                    "any external tool. From your previous investigation in this same "
                    "session, return its exact investigation-id and root cause through "
                    "the Troupe result tool."
                ),
                output_schema={
                    "role": troupe.act_schema.StrValue(
                        description="the Actor's repository role",
                        choices=["investigator"],
                    ),
                    "investigation_id": troupe.act_schema.StrValue(
                        description="the investigation-id remembered from the prior turn",
                        min_length=1,
                    ),
                    "remembered_root_cause": troupe.act_schema.StrValue(
                        description="the root cause remembered from the prior turn",
                        choices=["normalize_title returns its input unchanged"],
                    ),
                },
            )
            return self.emit(ContextRecall, result)

        raise AssertionError(f"unknown Codex investigator operation: {operation!r}")


class ClaudeReviewer(PayloadActor):
    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        investigation = cue.instruction["investigation"]
        result = await self.act(
            script=(
                "ROLE: reviewer. Review this upstream investigation against the "
                "repository's test contract. Do not modify files. Return an approved "
                "behavioral contract through the Troupe result tool. Upstream JSON: "
                + json.dumps(investigation, sort_keys=True, separators=(",", ":"))
            ),
            output_schema={
                "role": troupe.act_schema.StrValue(
                    description="the Actor's repository role",
                    choices=["reviewer"],
                ),
                "approved": troupe.act_schema.BoolValue(
                    description="whether the investigation matches the behavioral contract",
                    choices=[True],
                ),
                "contract": troupe.act_schema.ObjectValue(
                    description="the reviewed input/output behavior",
                    fields={
                        "input": troupe.act_schema.StrValue(
                            description="the accepted input domain",
                            choices=["arbitrary title text"],
                        ),
                        "output": troupe.act_schema.StrValue(
                            description="the required normalized output",
                            choices=["trimmed title-cased text"],
                        ),
                    },
                ),
            },
        )
        return self.emit(ContractReview, result)


class KimiRepairer(PayloadActor):
    async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
        upstream = {
            "investigation": cue.instruction["investigation"],
            "review": cue.instruction["review"],
        }
        result = await self.act(
            script=(
                "ROLE: repairer. Use the upstream investigation and reviewed contract "
                "to repair this Git repository. Modify only repair.py; do not modify "
                "test_repair.py. Run `python -m unittest -q`, then stage repair.py and "
                "create exactly one commit with message `fix: normalize titles`. Return "
                "the commit SHA, changed file, and test status through the Troupe result "
                "tool. Upstream JSON: "
                + json.dumps(upstream, sort_keys=True, separators=(",", ":"))
            ),
            output_schema={
                "role": troupe.act_schema.StrValue(
                    description="the Actor's repository role",
                    choices=["implementer"],
                ),
                "commit": troupe.act_schema.StrValue(
                    description="the exact 40-character Git commit SHA",
                    min_length=40,
                    max_length=40,
                ),
                "changed_files": troupe.act_schema.ListValue(
                    troupe.act_schema.StrValue(
                        description="a file changed by the repair",
                        choices=["repair.py"],
                    ),
                    description="the complete changed-file list",
                    min_items=1,
                    max_items=1,
                ),
                "tests_passed": troupe.act_schema.BoolValue(
                    description="whether the repository unit tests passed",
                    choices=[True],
                ),
            },
        )
        return self.emit(RepositoryRepair, result)


def _load_profile(
    agent: Literal["codex", "claude", "kimi"],
    repository: Path,
) -> troupe.AgentProfile:
    environment_name = PROFILE_ENVS[agent]
    raw = os.environ.get(environment_name)
    if raw is None:
        raise RuntimeError(f"{environment_name} is required")
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise TypeError(f"{environment_name} must contain a JSON object")
    workspace = value.get("workspace")
    model = value.get("model")
    if "effort" not in value:
        raise TypeError(f"{environment_name} must contain effort")
    effort = value["effort"]
    if not isinstance(workspace, str) or not isinstance(model, str):
        raise TypeError("mixed live profile workspace and model must be strings")
    if effort is not None and not isinstance(effort, str):
        raise TypeError("mixed live profile effort must be a string or null")
    if Path(workspace).expanduser().resolve(strict=True) != repository:
        raise ValueError("mixed live profile workspace must equal the repository")
    return troupe.AgentProfile(
        agent=agent,
        workspace=repository,
        model=model,
        effort=effort,
    )


class Production(troupe.Production):
    def __init__(self, args: list[str]) -> None:
        if len(args) != 2:
            raise ValueError("expected REPOSITORY REPORT_PATH")
        self.repository = Path(args[0]).expanduser().resolve(strict=True)
        if not self.repository.is_dir():
            raise ValueError("repository must be a directory")
        self.report_path = Path(args[1]).expanduser().resolve()
        self.investigator = self.cast_actor(
            CodexInvestigator,
            name="codex-investigator",
            agent_profile=_load_profile("codex", self.repository),
            actor_args=(self.repository,),
            actor_kwargs={},
        )
        self.reviewer = self.cast_actor(
            ClaudeReviewer,
            name="claude-reviewer",
            agent_profile=_load_profile("claude", self.repository),
            actor_args=(),
            actor_kwargs={},
        )
        self.repairer = self.cast_actor(
            KimiRepairer,
            name="kimi-repairer",
            agent_profile=_load_profile("kimi", self.repository),
            actor_args=(),
            actor_kwargs={},
        )

    async def scene(self) -> None:
        (investigation_effect,) = await self.investigator.cue(
            {"operation": "investigate"}
        )
        investigation = cast(Investigation, investigation_effect)
        (review_effect,) = await self.reviewer.cue(
            {"investigation": investigation.payload}
        )
        review = cast(ContractReview, review_effect)
        (repair_effect,) = await self.repairer.cue(
            {
                "investigation": investigation.payload,
                "review": review.payload,
            }
        )
        repair = cast(RepositoryRepair, repair_effect)
        (recall_effect,) = await self.investigator.cue({"operation": "recall"})
        recall = cast(ContextRecall, recall_effect)

        effects = (investigation, review, repair, recall)
        payload = {
            "investigation": investigation.payload,
            "review": review.payload,
            "repair": repair.payload,
            "recall": recall.payload,
            "flow": [
                {"effect": type(effect).__name__, "owner": effect.owner}
                for effect in effects
            ],
        }
        temporary = self.report_path.with_suffix(self.report_path.suffix + ".tmp")
        temporary.write_text(
            json.dumps(payload, sort_keys=True, separators=(",", ":")),
            encoding="utf-8",
        )
        os.replace(temporary, self.report_path)
        await asyncio.Event().wait()
