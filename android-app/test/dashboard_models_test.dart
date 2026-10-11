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
      'parent_pane_id': '%2',
      'tags': ['mobile'],
      'created_at': 10,
      'last_active': 12,
      'transcript_path': '/tmp/transcript.jsonl',
    });
    expect(pane.id, '%8');
    expect(pane.title, 'Fix tests');
    expect(pane.detailedState, 'busy_tool_use');
    expect(pane.tags, ['mobile']);
    expect(pane.parentPaneId, '%2');
    expect(pane.createdAt, 10);
    expect(pane.transcriptPath, '/tmp/transcript.jsonl');
  });

  test('transcript text prefers common readable payload fields', () {
    final entry = TranscriptEntry.fromJson({
      'event_type': 'assistant',
      'cursor': 20,
      'payload': {
        'text': 'done',
        'timestamp': '2026-10-11T00:00:00Z',
        'ignored': true,
      },
    });
    expect(entry.text, 'done');
    expect(entry.cursor, 20);
    expect(entry.timestamp, '2026-10-11T00:00:00Z');
  });

  test('transcript text flattens structured message content', () {
    final entry = TranscriptEntry.fromJson({
      'event_type': 'assistant',
      'source': 'transcript',
      'cursor': 21,
      'payload': {
        'message': {
          'content': [
            {'type': 'text', 'text': 'first'},
            {'type': 'text', 'text': 'second'},
          ],
        },
      },
    });
    expect(entry.text, 'first\nsecond');
  });

  test('structured daemon and client status retain management metadata', () {
    final status = DaemonStatus.fromJson({
      'uptime_secs': 90,
      'subscriber_count': 2,
      'config_generation': 3,
      'pending_restore': 4,
    });
    final info = ClientInfo.fromJson({
      'version': '0.1.0',
      'commit': 'abc123',
      'config_path': '/etc/slopd.toml',
      'socket_path': '/run/slopd.sock',
      'capabilities': ['run', 'listen'],
    });
    expect(status.pendingRestore, 4);
    expect(status.configGeneration, 3);
    expect(info.capabilities, ['run', 'listen']);
    expect(info.socketPath, '/run/slopd.sock');
  });
}
