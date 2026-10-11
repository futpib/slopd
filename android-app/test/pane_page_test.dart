import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:slopd_control/control.dart';
import 'package:slopd_control/models.dart';
import 'package:slopd_control/pane_page.dart';
import 'package:slopd_control/ui.dart';

void main() {
  testWidgets('pane presents a conversation with collapsible agent work', (
    tester,
  ) async {
    final pane = PaneInfo.fromJson({
      'pane_id': '%7',
      'pane_title': 'Fix the Android client',
      'state': 'idle',
      'detailed_state': 'idle',
      'working_dir': '/srv/slopd',
      'account': 'default',
      'backend': 'codex',
      'session_id': 'session-7',
      'tags': ['android'],
      'created_at': 10,
      'last_active': 12,
      'transcript_path': '/tmp/session.jsonl',
    });
    final fake = _FakeControlApi(pane);
    final connection = ControlConnection(fake, () async => fake);

    await tester.pumpWidget(
      MaterialApp(
        theme: appTheme(Brightness.dark),
        home: PanePage(api: connection, pane: pane),
      ),
    );
    await tester.pump(const Duration(milliseconds: 100));

    expect(find.text('You'), findsOneWidget);
    expect(find.text('Agent'), findsOneWidget);
    expect(find.text('Make this feel like chat.'), findsOneWidget);
    expect(find.text('It now reads like a conversation.'), findsOneWidget);
    expect(find.text('3 steps'), findsOneWidget);
    expect(find.text('command output'), findsNothing);
    expect(find.textContaining('{"text"'), findsNothing);
    expect(find.widgetWithText(TextField, 'Message agent'), findsOneWidget);

    await tester.tap(find.text('3 steps'));
    await tester.pumpAndSettle();
    expect(find.text('command output'), findsOneWidget);
    expect(find.textContaining('name: exec_command'), findsOneWidget);
    expect(find.textContaining('cmd: flutter test'), findsOneWidget);
    expect(find.textContaining('{"name"'), findsNothing);

    await tester.enterText(
      find.widgetWithText(TextField, 'Message agent'),
      'Follow up',
    );
    await tester.pump();
    await tester.tap(find.byTooltip('Send message'));
    await tester.pumpAndSettle();
    expect(fake.commands.last, ['send', '--timeout', '60', '%7', 'Follow up']);

    await tester.pumpWidget(const SizedBox.shrink());
    await tester.pump();
    await connection.close();
  });
}

class _FakeControlApi implements ControlApi {
  _FakeControlApi(this.pane);

  final PaneInfo pane;
  final commands = <List<String>>[];
  final _live = StreamController<String>();

  @override
  Future<CommandResult> command(
    List<String> arguments, {
    Duration timeout = const Duration(seconds: 45),
  }) async {
    commands.add(arguments);
    if (arguments.first == 'transcript') {
      return CommandResult(
        jsonEncode({
          'records': [
            {
              'cursor': 1,
              'event_type': 'userMessage',
              'payload': {'text': 'Make this feel like chat.'},
            },
            {
              'cursor': 2,
              'event_type': 'reasoning',
              'payload': {'text': 'Reviewing the current screen'},
            },
            {
              'cursor': 3,
              'event_type': 'toolCall',
              'payload': {
                'name': 'exec_command',
                'arguments': {'cmd': 'flutter test'},
              },
            },
            {
              'cursor': 4,
              'event_type': 'toolResult',
              'payload': {'output': 'command output'},
            },
            {
              'cursor': 5,
              'event_type': 'agentMessage',
              'payload': {'text': 'It now reads like a conversation.'},
            },
          ],
        }),
        '',
        0,
      );
    }
    if (arguments.first == 'ps') {
      return CommandResult(jsonEncode([_paneJson(pane)]), '', 0);
    }
    return const CommandResult('', '', 0);
  }

  @override
  Future<CommandStream> openStream(List<String> arguments) async =>
      CommandStream(
        lines: _live.stream,
        errors: const Stream.empty(),
        done: _live.done.then((_) => 0),
        close: () => _live.close(),
      );

  @override
  Future<void> close() async {
    if (!_live.isClosed) await _live.close();
  }
}

Map<String, dynamic> _paneJson(PaneInfo pane) => {
  'pane_id': pane.id,
  'pane_title': pane.title,
  'state': pane.state,
  'detailed_state': pane.detailedState,
  'working_dir': pane.directory,
  'account': pane.account,
  'backend': pane.backend,
  'session_id': pane.sessionId,
  'parent_pane_id': pane.parentPaneId,
  'tags': pane.tags,
  'created_at': pane.createdAt,
  'last_active': pane.lastActive,
  'transcript_path': pane.transcriptPath,
};
