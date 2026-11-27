use super::*;
use chrono::{TimeZone, Utc};

fn hook_for_group(id: &str, event: &str, group_name: &str) -> TranscriptEntry {
    let phase = match event {
        "hook_started" => "started",
        "hook_response" => "completed",
        _ => event,
    };
    TranscriptEntry::new(
        TranscriptEntryId::from(id),
        vec![event.to_owned()],
        format!("{{\"hook_id\":\"{id}\"}}").into_bytes(),
    )
    .with_header("SYSTEM · Harness · 2026-10-11 12:00:00".to_string())
    .with_presentation(TranscriptPresentation::System)
    .with_operation_grouping(Some("session"), Some(group_name), Some(phase), Some(id))
}

fn hook(id: &str, event: &str) -> TranscriptEntry {
    hook_for_group(id, event, "SessionStart")
}

fn tool(id: &str, name: &str, phase: &str) -> TranscriptEntry {
    TranscriptEntry::new(
        TranscriptEntryId::from(format!("tool-{id}-{phase}")),
        vec![format!("{phase}: {name}")],
        format!("{{\"id\":\"{id}\",\"phase\":\"{phase}\"}}").into_bytes(),
    )
    .with_header("TECHNICZNE · Harness · 2026-10-11 12:00:00")
    .with_presentation(TranscriptPresentation::Technical)
    .with_correlated_operation(Some("session"), Some(name), Some(phase), Some(id))
}

#[test]
fn conversation_groups_all_contiguous_hook_events() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        hook("one", "hook_started"),
        hook("two", "hook_started"),
        hook("two", "hook_response"),
        hook("one", "hook_response"),
    ]);

    let rendered = transcript.logical_lines();
    assert!(rendered
        .iter()
        .any(|line| line.text.starts_with("› SYSTEM · Harness")));
    assert!(rendered
        .iter()
        .any(|line| line.text == "Hooki · SessionStart · 2 zakończone"));
    assert_eq!(rendered.iter().filter(|line| line.header).count(), 1);

    let group = transcript.selected_entry().cloned().unwrap();
    transcript.toggle_expanded(&group);
    let expanded = transcript.logical_lines();
    assert!(expanded
        .iter()
        .any(|line| line.text.starts_with("⌄ SYSTEM · Harness")));
    assert!(expanded
        .iter()
        .any(|line| line.text == "Hooki · SessionStart · 2 zakończone"));
    assert!(expanded
        .iter()
        .any(|line| line.text.starts_with("12:00:00 · hook_started")));
    assert_eq!(
        expanded
            .iter()
            .filter(|line| line.text.contains("SYSTEM · Harness"))
            .count(),
        1
    );

    transcript.next_conversation_view();
    let chronology = transcript.logical_lines();
    assert_eq!(chronology.iter().filter(|line| line.header).count(), 4);
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("one"))
    );
}

#[test]
fn grouped_error_member_keeps_a_compact_error_attribution() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        hook("one", "hook_started"),
        hook("two", "hook_response").with_presentation(TranscriptPresentation::Error),
    ]);
    let group = transcript.selected_entry().cloned().unwrap();
    transcript.toggle_expanded(&group);
    assert!(transcript
        .logical_lines()
        .iter()
        .any(|line| line.text.contains("BŁĄD · hook_response")));
}

#[test]
fn conversation_pairs_interleaved_operations_by_id() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        tool("one", "bash", "started"),
        tool("two", "search", "started"),
        tool("two", "search", "completed"),
        tool("one", "bash", "completed"),
    ]);

    let rendered = transcript.logical_lines();
    assert!(rendered
        .iter()
        .any(|line| line.text.starts_with("› TECHNICZNE · Harness")));
    assert!(rendered
        .iter()
        .any(|line| line.text == "Narzędzie · bash · zakończone"));
    assert!(rendered
        .iter()
        .any(|line| line.text == "Narzędzie · search · zakończone"));
    assert_eq!(rendered.iter().filter(|line| line.header).count(), 2);
}

#[test]
fn operation_summary_exposes_a_result_error_without_expansion() {
    let mut result = tool("one", "bash", "completed");
    result.details = Some(serde_json::json!({"error.message": "permission denied"}));
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![tool("one", "bash", "started"), result]);

    assert!(transcript
        .logical_lines()
        .iter()
        .any(|line| line.text == "Narzędzie · bash · zakończone · błąd"));
}

#[test]
fn local_raw_from_chronology_freezes_future_group_members() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        hook("one", "hook_started"),
        hook("two", "hook_started"),
    ]);
    transcript.next_conversation_view();
    transcript.line_up();
    transcript.toggle_selected_raw();
    let mut response = hook("two", "hook_response");
    response.id = TranscriptEntryId::from("two-response");
    response.source_ids = vec![TranscriptEntryId::from("two-response")];
    transcript.append(vec![response]);
    transcript.set_mode(DisplayMode::Rendered);

    let rendered = transcript.logical_lines();
    assert_eq!(
        rendered
            .iter()
            .map(|line| &line.entry_id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        2
    );
}

#[test]
fn chronology_keeps_the_same_second_precision_header_as_conversation() {
    let at = Utc.with_ymd_and_hms(2026, 10, 11, 12, 34, 56).unwrap();
    let entry = TranscriptEntry::new(
        TranscriptEntryId::from("one"),
        vec!["message".into()],
        Vec::new(),
    )
    .with_header(utterance_header(TranscriptPresentation::Agent, "Agent", at))
    .with_presentation(TranscriptPresentation::Agent);
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![entry]);
    let conversation_header = transcript.logical_lines()[0].text.clone();
    transcript.next_conversation_view();
    assert_eq!(transcript.logical_lines()[0].text, conversation_header);
}

#[test]
fn disclosure_marker_only_appears_when_expansion_adds_content() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        TranscriptEntry::new(
            TranscriptEntryId::from("same"),
            vec!["already visible".into()],
            Vec::new(),
        )
        .with_detail_lines(vec!["already visible".into()]),
        TranscriptEntry::new(
            TranscriptEntryId::from("extra"),
            vec!["summary".into()],
            Vec::new(),
        )
        .with_detail_lines(vec!["additional detail".into()]),
    ]);
    transcript.line_up();
    assert!(transcript
        .logical_lines()
        .iter()
        .filter(|line| line.entry_id == TranscriptEntryId::from("same"))
        .filter(|line| line.header)
        .all(|line| line.text.starts_with("· ")));
    transcript.line_down();
    assert!(transcript
        .logical_lines()
        .iter()
        .any(|line| line.text == "› "));
    let id = transcript.selected_entry().cloned().unwrap();
    transcript.toggle_expanded(&id);
    assert_eq!(
        transcript
            .logical_lines()
            .iter()
            .filter(|line| line.text == "additional detail")
            .count(),
        1
    );
}

#[test]
fn disclosure_state_is_in_header_and_every_body_line_is_indented() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        TranscriptEntry::new(
            TranscriptEntryId::from("expandable"),
            vec!["first".into(), "second".into()],
            Vec::new(),
        )
        .with_header("AGENT · Agent")
        .with_detail_lines(vec!["extra".into()]),
        TranscriptEntry::new(
            TranscriptEntryId::from("plain"),
            vec!["plain body".into()],
            Vec::new(),
        )
        .with_header("SYSTEM · Harness"),
    ]);

    let collapsed = transcript.logical_lines();
    assert_eq!(collapsed[0].text, "› AGENT · Agent");
    assert_eq!(collapsed[1].text, "first");
    assert_eq!(collapsed[2].text, "second");
    assert!(collapsed[1].indented && collapsed[2].indented);
    assert!(!collapsed[0].indented);
    assert_eq!(collapsed[4].text, "· SYSTEM · Harness");
    assert_eq!(collapsed[5].text, "plain body");
    assert!(collapsed[5].indented);

    transcript.toggle_expanded(&TranscriptEntryId::from("expandable"));
    let expanded = transcript.logical_lines();
    assert_eq!(expanded[0].text, "⌄ AGENT · Agent");
    assert_eq!(expanded[3].text, "extra");
    assert!(expanded[3].indented);
}

#[test]
fn duplicate_tail_lines_still_show_a_disclosure_marker_and_expand() {
    let body = vec![
        "Status:".to_owned(),
        "ok".to_owned(),
        "ok".to_owned(),
        "ok".to_owned(),
        "ok".to_owned(),
    ];
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![TranscriptEntry::new(
        TranscriptEntryId::from("repeated"),
        body,
        Vec::new(),
    )
    .with_detail_lines(vec!["ok".into(), "ok".into()])]);
    let id = transcript.selected_entry().cloned().unwrap();
    assert!(transcript
        .logical_lines()
        .iter()
        .any(|line| line.text == "› "));

    transcript.toggle_expanded(&id);
    assert_eq!(
        transcript
            .logical_lines()
            .iter()
            .filter(|line| line.text == "ok")
            .count(),
        6
    );
}

#[test]
fn wrapped_body_continuations_keep_the_two_space_indent() {
    let lines = [TranscriptLine {
        entry_id: TranscriptEntryId::from("wrapped"),
        logical_line: 0,
        text: "abcdefghijk".into(),
        presentation: TranscriptPresentation::Agent,
        header: false,
        indented: true,
        nested: false,
        selected: true,
        workflow_choice: false,
        clipped: false,
    }];
    let wrapped = wrap_visible_lines(&lines, 8, 0, 3);
    assert_eq!(
        wrapped
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>(),
        ["abcdef", "ghijk"]
    );
}

#[test]
fn bottom_scroll_reaches_the_last_wrapped_indented_line() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.set_width(10);
    transcript.set_viewport(3);
    transcript.load(vec![TranscriptEntry::new(
        TranscriptEntryId::from("long"),
        vec![
            "abcdefghij".into(),
            "klmnopqrst".into(),
            "uvwxyzABCD".into(),
        ],
        Vec::new(),
    )]);

    let logical = transcript.logical_lines();
    let visible = wrap_visible_lines(&logical, 10, transcript.scroll_offset(), 3);
    assert_eq!(visible.len(), 3);
    assert_eq!(visible.last().unwrap().text, "CD");
    assert!(visible.last().unwrap().indented);
}

#[test]
fn conversation_keeps_different_hook_families_separate() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        hook_for_group("one", "hook_started", "SessionStart"),
        hook_for_group("two", "hook_response", "OtherLifecycle"),
        hook_for_group("one", "hook_response", "SessionStart"),
    ]);

    let rendered = transcript.logical_lines();
    assert_eq!(rendered.iter().filter(|line| line.header).count(), 3);
    assert!(rendered
        .iter()
        .any(|line| line.text.ends_with("hook_started")));
}

#[test]
fn conversation_groups_adjacent_hooks_even_when_one_is_unpaired() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        hook("unpaired", "hook_started"),
        hook("paired", "hook_started"),
        hook("paired", "hook_response"),
    ]);

    let rendered = transcript.logical_lines();
    assert_eq!(rendered.iter().filter(|line| line.header).count(), 1);
    assert!(rendered
        .iter()
        .any(|line| line.text.starts_with("› SYSTEM · Harness")));
    assert!(rendered
        .iter()
        .any(|line| line.text == "Hooki · SessionStart · 1 zakończone · 1 w toku"));
}

#[test]
fn revealed_group_does_not_absorb_later_members() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        hook("one", "hook_started"),
        hook("two", "hook_started"),
    ]);
    let group = transcript.selected_entry().cloned().unwrap();
    transcript.toggle_expanded(&group);
    transcript.append(vec![hook("three", "hook_started")]);

    let lines: Vec<_> = transcript
        .logical_lines()
        .into_iter()
        .map(|line| line.text)
        .collect();
    assert!(lines.iter().any(|line| line.ends_with("hook_started")));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.ends_with("hook_started"))
            .count(),
        3
    );
    assert!(lines
        .iter()
        .any(|line| line.starts_with("⌄ SYSTEM · Harness")));
}

#[test]
fn closing_local_raw_keeps_an_expanded_group_frozen() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        hook("one", "hook_started"),
        hook("two", "hook_started"),
    ]);
    let group = transcript.selected_entry().cloned().unwrap();
    transcript.toggle_expanded(&group);
    transcript.toggle_selected_raw();
    transcript.toggle_selected_raw();
    transcript.append(vec![hook("three", "hook_started")]);

    assert_eq!(
        transcript
            .logical_lines()
            .iter()
            .filter(|line| line.text.ends_with("hook_started"))
            .count(),
        3
    );
}

#[test]
fn hooks_without_correlation_remain_separate() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    let uncorrelated = |id: &str| {
        TranscriptEntry::new(TranscriptEntryId::from(id), vec![id.into()], Vec::new())
            .with_operation_grouping(Some("session"), Some("SessionStart"), Some("started"), None)
    };
    transcript.load(vec![uncorrelated("one"), uncorrelated("two")]);
    assert_eq!(
        transcript
            .logical_lines()
            .iter()
            .filter(|line| line.header)
            .count(),
        2
    );
    assert_eq!(
        transcript
            .logical_lines()
            .iter()
            .filter(|line| line.text == "one" || line.text == "two")
            .count(),
        2
    );
}

#[test]
fn refresh_preserves_the_source_selected_inside_a_group() {
    let entries = vec![hook("one", "hook_started"), hook("two", "hook_started")];
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(entries.clone());
    transcript.set_mode(DisplayMode::Chronology);
    transcript.line_down();
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("two"))
    );

    transcript.set_mode(DisplayMode::Rendered);
    transcript.replace_entries(entries);
    transcript.set_mode(DisplayMode::Chronology);
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("two"))
    );
}

fn choice(id: &str, lines: Vec<String>) -> TranscriptEntry {
    TranscriptEntry::new(TranscriptEntryId::from(id), lines, b"{}".to_vec())
        .with_header("WYBÓR WORKFLOW · Agent")
        .with_presentation(TranscriptPresentation::Agent)
}

#[test]
fn every_card_line_of_a_workflow_choice_carries_the_flag() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        choice("choice", vec!["one".into(), "two".into()]).as_workflow_choice(),
        choice("plain", vec!["one".into()]),
    ]);

    let lines = transcript.logical_lines();
    assert!(lines[..3].iter().all(|line| line.workflow_choice));
    assert!(lines[3..].iter().all(|line| !line.workflow_choice));
}

/// R9: what is one line by nature is clipped with `…`; the prose wraps.
#[test]
fn clipped_heads_end_with_an_ellipsis_and_the_prose_wraps() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![choice(
        "choice",
        vec![
            "agent · claude wybrał opcję: bardzo_długa_etykieta".into(),
            "uzasadnienie: abcdefghijklmnopqrstuvwxyz".into(),
        ],
    )
    .with_clipped_head()
    .as_workflow_choice()]);

    let wrapped = wrap_visible_lines(&transcript.logical_lines(), 20, 0, 20);
    assert_eq!(wrapped[0].text.chars().count(), 20);
    assert!(wrapped[0].text.starts_with("· WYBÓR"));
    assert!(wrapped[0].text.ends_with('…'));
    assert!(wrapped[1].text.ends_with('…'));
    assert!(wrapped[2..].len() > 1, "the reason wraps: {wrapped:?}");
    assert!(wrapped[2..].iter().all(|line| !line.text.ends_with('…')));
    assert!(wrapped.iter().all(|line| line.workflow_choice));
}

#[test]
fn heads_that_are_not_marked_wrap_as_they_did() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![choice("plain", vec!["one".into()])]);
    let wrapped = wrap_visible_lines(&transcript.logical_lines(), 10, 0, 20);
    assert!(wrapped.len() > 2);
    assert!(wrapped.iter().all(|line| !line.text.ends_with('…')));
}

#[test]
fn every_wrapped_fragment_of_a_workflow_choice_keeps_the_flag() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![choice(
        "choice",
        vec!["abcdefghijklmnopqrstuvwxyz".into()],
    )
    .as_workflow_choice()]);

    let wrapped = wrap_visible_lines(&transcript.logical_lines(), 10, 0, 20);
    assert!(wrapped.len() > 3);
    assert!(wrapped.iter().any(|line| !line.header && !line.indented));
    assert!(wrapped.iter().all(|line| line.workflow_choice));
}

#[test]
fn raw_views_of_a_workflow_choice_are_not_highlighted() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.load(vec![
        choice("choice", vec!["one".into()]).as_workflow_choice()
    ]);

    transcript.toggle_selected_raw();
    assert!(transcript
        .logical_lines()
        .iter()
        .all(|line| !line.workflow_choice));

    transcript.toggle_selected_raw();
    transcript.set_mode(DisplayMode::Raw);
    assert!(transcript
        .logical_lines()
        .iter()
        .all(|line| !line.workflow_choice));

    transcript.set_mode(DisplayMode::Chronology);
    assert!(transcript
        .logical_lines()
        .iter()
        .all(|line| line.workflow_choice));
}

#[test]
fn the_workflow_choice_flag_does_not_change_the_line_count() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.set_width(10);
    let body = vec!["abcdefghijklmnopqrstuvwxyz".to_owned(), "x".to_owned()];
    let plain = choice("plain", body.clone());
    let marked = choice("plain", body).as_workflow_choice();

    assert_eq!(
        transcript.visual_line_count_for(&plain),
        transcript.visual_line_count_for(&marked)
    );
}

/// An agent utterance whose body shows `preview` lines and whose speech
/// continues for `continuation` more, followed by `technical` detail lines.
fn utterance(id: &str, preview: usize, continuation: usize, technical: usize) -> TranscriptEntry {
    let speech = (0..continuation).map(|n| {
        TranscriptDetailLine::new(format!("{id} more {n}"), TranscriptPresentation::Agent)
    });
    let technical = (0..technical).map(|n| {
        TranscriptDetailLine::new(format!("{id} usage {n}"), TranscriptPresentation::Technical)
    });
    TranscriptEntry::new(
        TranscriptEntryId::from(id),
        (0..preview).map(|n| format!("{id} line {n}")).collect(),
        Vec::new(),
    )
    .with_header(format!("AGENT · {id}"))
    .with_presentation(TranscriptPresentation::Agent)
    .with_details(speech.chain(technical).collect())
    .with_speech_continuation(continuation)
}

fn shown(transcript: &TranscriptState, text: &str) -> bool {
    transcript
        .logical_lines()
        .iter()
        .any(|line| line.text == text)
}

fn following_reader() -> TranscriptState {
    TranscriptState::new(TranscriptId::from("attempt")).with_auto_expand_latest()
}

#[test]
fn following_shows_the_newest_utterance_in_full_and_collapses_the_previous_one() {
    // Covers AE1, AE2.
    let mut transcript = following_reader();
    transcript.load(vec![utterance("old", 5, 2, 0), utterance("first", 5, 7, 0)]);
    assert!(shown(&transcript, "first more 6"));

    transcript.to_top();
    let old = TranscriptEntryId::from("old");
    transcript.toggle_expanded(&old);
    transcript.toggle_follow();
    transcript.append(vec![utterance("second", 5, 7, 0)]);

    assert!(!shown(&transcript, "first more 0"), "first collapsed");
    assert!(
        shown(&transcript, "first line 4"),
        "first keeps its preview"
    );
    assert!(shown(&transcript, "second more 6"));
    assert!(shown(&transcript, "old more 1"), "a manual expansion stays");
}

#[test]
fn a_non_speech_newest_entry_is_not_expanded_and_ends_the_previous_auto_expansion() {
    // Covers AE3.
    let mut transcript = following_reader();
    transcript.load(vec![utterance("speech", 5, 7, 0)]);
    let mut tool = tool("t", "Bash", "completed");
    tool.detail_lines = vec![TranscriptDetailLine::new(
        "long output",
        TranscriptPresentation::Technical,
    )];
    transcript.append(vec![tool]);

    assert!(!shown(&transcript, "speech more 0"));
    assert!(!shown(&transcript, "long output"));
}

#[test]
fn following_a_tall_utterance_shows_its_end_with_the_cursor_on_it() {
    // Covers AE4.
    let mut transcript = following_reader();
    transcript.set_viewport(20);
    transcript.load(vec![utterance("earlier", 1, 0, 0)]);
    transcript.append(vec![utterance("tall", 5, 34, 0)]);

    let lines = transcript.logical_lines();
    let visible = wrap_visible_lines(&lines, 80, transcript.scroll_offset(), 20);
    assert_eq!(visible.len(), 20);
    assert_eq!(visible.last().unwrap().text, "tall more 33");
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("tall"))
    );
}

#[test]
fn leaving_follow_keeps_the_auto_expanded_utterance_open() {
    // Covers AE5.
    let mut transcript = following_reader();
    transcript.load(vec![utterance("a", 5, 0, 0), utterance("b", 5, 7, 0)]);
    transcript.line_up();
    assert!(!transcript.is_following_end());
    assert!(shown(&transcript, "b more 6"));

    // The operator sees it expanded, so a toggle collapses it.
    transcript.toggle_expanded(&TranscriptEntryId::from("b"));
    assert!(!shown(&transcript, "b more 0"));
}

#[test]
fn auto_expansion_reveals_speech_but_not_technical_details() {
    let mut transcript = following_reader();
    transcript.load(vec![utterance("result", 5, 3, 2)]);
    assert!(shown(&transcript, "result more 2"));
    assert!(!shown(&transcript, "result usage 0"));
}

#[test]
fn a_reader_without_the_policy_keeps_the_preview_of_the_newest_utterance() {
    let mut transcript = TranscriptState::new(TranscriptId::from("chat"));
    transcript.load(vec![utterance("u", 5, 7, 0)]);
    assert!(!shown(&transcript, "u more 0"));
}

#[test]
fn a_refresh_while_following_moves_the_cursor_to_the_newest_entry() {
    let mut transcript = following_reader();
    transcript.load(vec![utterance("a", 1, 0, 0)]);
    transcript.replace_entries(vec![utterance("a", 1, 0, 0), utterance("b", 5, 7, 0)]);
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("b"))
    );
    assert!(shown(&transcript, "b more 6"));
}

/// The last content row visible in a viewport of `height` rows.
fn last_visible_row(transcript: &TranscriptState, height: usize) -> String {
    let lines = transcript.logical_lines();
    wrap_visible_lines(&lines, 80, transcript.scroll_offset(), height)
        .last()
        .unwrap()
        .text
        .clone()
}

#[test]
fn a_viewport_height_change_keeps_the_last_visible_line_and_the_cursor() {
    // Covers AE6.
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.set_viewport(20);
    transcript.load(vec![utterance("a", 5, 0, 0), utterance("b", 60, 0, 0)]);
    transcript.to_top();
    transcript.line_down();
    transcript.page_down();
    let selected = transcript.selected_entry().cloned();
    let before = last_visible_row(&transcript, 20);

    transcript.set_viewport(17);
    assert_eq!(last_visible_row(&transcript, 17), before);
    assert_eq!(transcript.selected_entry().cloned(), selected);

    transcript.set_viewport(20);
    assert_eq!(last_visible_row(&transcript, 20), before);
}

#[test]
fn a_viewport_change_near_the_top_or_while_following_stays_in_bounds() {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.set_viewport(20);
    transcript.load(vec![utterance("a", 5, 0, 0), utterance("b", 30, 0, 0)]);
    assert_eq!(last_visible_row(&transcript, 20), "b line 29");
    transcript.set_viewport(14);
    assert_eq!(last_visible_row(&transcript, 14), "b line 29", "following");

    transcript.to_top();
    transcript.set_viewport(30);
    assert_eq!(transcript.scroll_offset(), 0);
    transcript.set_viewport(10);
    assert!(transcript.scroll_offset() <= 20);
}

#[test]
fn collapsing_a_manually_and_auto_expanded_utterance_hides_its_speech() {
    let mut transcript = following_reader();
    transcript.load(vec![utterance("a", 5, 0, 0), utterance("b", 5, 7, 0)]);
    let b = TranscriptEntryId::from("b");
    transcript.toggle_expanded(&b);
    transcript.toggle_expanded(&b);
    assert!(shown(&transcript, "b more 6"), "expanded by hand");
    transcript.toggle_follow();

    transcript.toggle_expanded(&b);

    assert!(!shown(&transcript, "b more 0"));
}

#[test]
fn following_in_raw_view_does_not_move_the_auto_expansion() {
    let mut transcript = following_reader();
    transcript.load(vec![utterance("a", 5, 7, 0)]);
    transcript.toggle_mode();
    transcript.toggle_follow();
    transcript.append(vec![
        utterance("b", 1, 0, 0),
        utterance("b.2", 5, 7, 0).sharing_previous_raw(),
    ]);
    transcript.toggle_mode();

    assert!(shown(&transcript, "a more 6"));
}

fn six_short_utterances() -> TranscriptState {
    let mut transcript = TranscriptState::new(TranscriptId::from("attempt"));
    transcript.set_viewport(5);
    transcript.load(
        (0..6)
            .map(|n| utterance(&format!("u{n}"), 1, 0, 0))
            .collect(),
    );
    transcript
}

#[test]
fn scrolling_by_lines_ends_the_following_and_stops_where_the_page_keys_stop() {
    let mut transcript = six_short_utterances();
    assert_eq!(transcript.scroll_offset(), 12);
    assert!(transcript.is_following_end());

    transcript.scroll_by(-3);
    assert_eq!(transcript.scroll_offset(), 9);
    assert!(!transcript.is_following_end());
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("u3")),
        "the cursor stays on an entry in view"
    );

    transcript.scroll_by(-100);
    assert_eq!(transcript.scroll_offset(), 0);

    let mut paged = transcript.clone();
    for _ in 0..5 {
        paged.page_down();
    }
    transcript.scroll_by(100);
    assert_eq!(transcript.scroll_offset(), paged.scroll_offset());
    transcript.scroll_by(3);
    assert_eq!(transcript.scroll_offset(), 12, "the bottom holds");
}

#[test]
fn a_visible_row_names_the_entry_drawn_on_it() {
    let mut transcript = six_short_utterances();
    transcript.scroll_by(-100);
    transcript.scroll_by(1);
    assert_eq!(
        transcript.entry_at_row(0),
        Some(TranscriptEntryId::from("u0"))
    );
    assert_eq!(
        transcript.entry_at_row(2),
        Some(TranscriptEntryId::from("u1"))
    );
    assert_eq!(transcript.entry_at_row(5), None, "below the window");

    assert!(transcript.select_row(2));
    assert_eq!(
        transcript.selected_entry(),
        Some(&TranscriptEntryId::from("u1"))
    );
    assert!(!transcript.select_row(5));
}
