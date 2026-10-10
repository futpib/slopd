import 'package:flutter_test/flutter_test.dart';
import 'package:slopd_control/dashboard.dart';

void main() {
  test('pane JSON keeps the remote slopctl fields used by the dashboard', () {
    final pane = PaneInfo.fromJson({
      'pane_id': '%8',
      'pane_title': 'Fix tests',
      'state': 'busy',
      'detailed_state': 'busy_tool_use',
      'working_dir': '/srv/project',
      'account': 'work',
      'backend': 'codex',
      'session_id': 'session',
      'tags': ['mobile'],
      'last_active': 12,
    });
    expect(pane.id, '%8');
    expect(pane.title, 'Fix tests');
    expect(pane.detailedState, 'busy_tool_use');
    expect(pane.tags, ['mobile']);
  });

  test('transcript text prefers common readable payload fields', () {
    final entry = TranscriptEntry.fromJson({
      'event_type': 'assistant',
      'cursor': 20,
      'payload': {'text': 'done', 'ignored': true},
    });
    expect(entry.text, 'done');
    expect(entry.cursor, 20);
  });
}
