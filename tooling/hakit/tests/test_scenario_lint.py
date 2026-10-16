"""The static scenario lint (ADR-0049 §4.2): each rule refuses a scenario that breaks it."""

from __future__ import annotations

from pathlib import Path

import pytest

from hakit.content import discover
from hakit.paths import BASE_CONTENT
from hakit.scenario_lint import lint, lint_all, scenario_keys
from hakit.schemas import SchemaSet

# A small UAC that keeps every rule: a bounded setup wait, retransmitted sends
# with ontimeout, one record on every path, a handler taking everything.
GOOD = """<?xml version="1.0" encoding="ISO-8859-1" ?>
<scenario name="lint fixture">
  <send retrans="500" ontimeout="no_final"><![CDATA[INVITE sip:[field0]@[remote_ip] SIP/2.0
X-Run: [run]
]]></send>
  <label id="wait"/>
  <recv response="100" optional="true" timeout="1000" ontimeout="no_final" next="wait"/>
  <recv response="200" rrs="true" next="answered"/>
  <nop/>
  <label id="no_final"/>
  <nop next="record"/>
  <nop/>
  <label id="answered"/>
  <send retrans="500" ontimeout="record"><![CDATA[BYE [next_url] SIP/2.0]]></send>
  <recv response="200" next="record"/>
  <nop next="record"/>
  <label id="_unexp.main"/>
  <recv request="[A-Z]" regexp_match="true" optional="true" next="resume"/>
  <recv response="^[1-6][0-9][0-9]$" regexp_match="true" next="resume"/>
  <nop/>
  <label id="resume"/>
  <nop>
    <action>
      <jump variable="_unexp.retaddr"/>
    </action>
  </nop>
  <label id="record"/>
  <nop>
    <action>
      <log message="{&quot;schema&quot;:&quot;call-outcome.v1&quot;,&quot;hold&quot;:[hold_ms]}"/>
    </action>
  </nop>
</scenario>
"""


def errors_of(tmp_path: Path, text: str) -> list[str]:
    p = tmp_path / "uac-x.xml"
    p.write_text(text)
    return [e.removeprefix(f"{p}: ") for e in lint(p)]


def test_the_fixture_keeps_every_rule(tmp_path: Path) -> None:
    assert errors_of(tmp_path, GOOD) == []


def test_every_scenario_on_the_base_path_keeps_every_rule() -> None:
    content = discover([BASE_CONTENT], SchemaSet.load())
    assert [n for n in content.scenarios if n.endswith(".xml")]
    assert lint_all(content.scenarios) == []


@pytest.mark.parametrize(
    ("old", "new", "error"),
    [
        (
            '<send retrans="500" ontimeout="record">',
            '<send retrans="500">',
            "step 6: a retransmitted send has no ontimeout",
        ),
        (
            'timeout="1000" ontimeout="no_final" next="wait"',
            'next="wait"',
            "step 1: the wait is bounded by no timeout and no retransmitted send",
        ),
        (
            'timeout="1000" ontimeout="no_final" next="wait"',
            'timeout="1000" next="wait"',
            "step 1: a recv with a timeout has no ontimeout",
        ),
        (
            '<recv response="100" optional="true" timeout="1000" ontimeout="no_final" next="wait"/>',
            '<recv response="100" optional="true" timeout="1000" ontimeout="no_final"/>',
            "step 1: an optional recv names no next",
        ),
        (
            '<recv response="^[1-6][0-9][0-9]$"',
            '<recv response="^[1-5][0-9][0-9]$"',
            "_unexp.main takes no response 600 (and 99 more)",
        ),
        (
            '<recv request="[A-Z]" regexp_match="true"',
            '<recv request="OPTIONS|BYE" regexp_match="true"',
            "_unexp.main takes no request INVITE, ACK, CANCEL, INFO, UPDATE, PRACK, REFER, NOTIFY, MESSAGE",
        ),
        (
            '  <nop/>\n  <label id="no_final"/>',
            '  <label id="no_final"/>',
            "label 'no_final' follows step 2, whose unconditional next a resume would apply",
        ),
        (
            '<nop next="record"/>\n  <label id="_unexp.main"/>',
            '<nop/>\n  <label id="_unexp.main"/>',
            "step 8 falls through into _unexp.main",
        ),
        (
            '<nop next="record"/>',
            '<nop next="resume_none"/>',
            "step 4: next names no label 'resume_none'",
        ),
    ],
    ids=[
        "send-ontimeout",
        "wait-bound",
        "recv-ontimeout",
        "optional-next",
        "handler-responses",
        "handler-requests",
        "resume-guard",
        "handler-fallthrough",
        "dangling-label",
    ],
)
def test_a_scenario_breaking_a_rule_is_refused(tmp_path: Path, old: str, new: str, error: str) -> None:
    assert old in GOOD
    assert error in errors_of(tmp_path, GOOD.replace(old, new, 1))


def test_a_path_without_a_record_is_refused(tmp_path: Path) -> None:
    text = GOOD.replace('<nop next="record"/>', '<nop next="end"/>').replace("</scenario>", '<label id="end"/>\n</scenario>')
    assert "the call can end without a record (after step 4 (nop, label no_final))" in errors_of(tmp_path, text)


def test_a_path_writing_two_records_is_refused(tmp_path: Path) -> None:
    text = GOOD.replace('<label id="record"/>', '<label id="record"/>\n  <nop><action><log message="call-outcome.v1"/></action></nop>')
    assert any("writes a second record on one path" in e for e in errors_of(tmp_path, text))


def test_a_scenario_writing_no_record_is_refused(tmp_path: Path) -> None:
    assert "no step writes a call-outcome record" in errors_of(tmp_path, GOOD.replace("call-outcome.v1", "other"))


def test_a_uas_writes_no_record_and_needs_no_handler(tmp_path: Path) -> None:
    uas = """<scenario name="uas">
  <recv request="INVITE"/>
  <send retrans="500" ontimeout="done"><![CDATA[SIP/2.0 200 OK]]></send>
  <recv request="ACK" next="done"/>
  <label id="done"/>
  <nop/>
</scenario>"""
    assert errors_of(tmp_path, uas) == []
    assert errors_of(tmp_path, uas.replace(' ontimeout="done"', "")) == ["step 1: a retransmitted send has no ontimeout"]
    assert errors_of(tmp_path, uas.replace("  <nop/>\n", "")) == ["step 1: ontimeout names the end, where SIPp counts the call failed"]


def test_the_keys_a_scenario_reads_leave_out_sipp_keywords_and_regexps(tmp_path: Path) -> None:
    p = tmp_path / "k.xml"
    p.write_text(
        GOOD.replace("</scenario>", '<nop><action><ereg regexp="[[:cntrl:]][Vv]" search_in="msg" assign_to="v"/>'
        '<assignstr assign_to="x" value="[ring_ms] [$v] [field1] [last_Via:]"/></action></nop>\n</scenario>')
    )
    assert scenario_keys(p) == {"run", "hold_ms", "ring_ms"}
