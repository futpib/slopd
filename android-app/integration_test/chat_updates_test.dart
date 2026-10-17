import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';
import 'package:slopd_control/control.dart';
import 'package:slopd_control/models.dart';
import 'package:slopd_control/pane_page.dart';

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets(
    'chat recovers updates across Android lifecycle and stream loss',
    (tester) async {
      final fake = _ChatApi();
      final connection = ControlConnection(fake, () async => fake);
      await tester.pumpWidget(
        MaterialApp(
          home: PanePage(api: connection, pane: fake.pane),
        ),
      );
      await tester.pump(const Duration(milliseconds: 200));
      expect(find.text('First answer'), findsOneWidget);

      fake.add(3, 'userMessage', 'Next question');
      await tester.pump(const Duration(milliseconds: 200));
      expect(find.text('Next question'), findsOneWidget);

      for (final state in [
        AppLifecycleState.inactive,
        AppLifecycleState.hidden,
        AppLifecycleState.paused,
      ]) {
        tester.binding.handleAppLifecycleStateChanged(state);
      }
      fake.add(4, 'agentMessage', 'Answered while away');

      for (final state in [
        AppLifecycleState.hidden,
        AppLifecycleState.inactive,
        AppLifecycleState.resumed,
      ]) {
        tester.binding.handleAppLifecycleStateChanged(state);
      }
      await tester.pump(const Duration(milliseconds: 200));
      expect(find.text('Answered while away'), findsOneWidget);

      fake.liveAvailable = false;
      await fake.endLive();
      fake.add(5, 'userMessage', 'Question with stream down');
      await tester.pump(const Duration(seconds: 3));
      await tester.pump();
      expect(find.text('Question with stream down'), findsOneWidget);

      fake.liveAvailable = true;
      await tester.pump(const Duration(seconds: 3));
      expect(fake.liveStarts, greaterThan(1));

      await tester.pumpWidget(const SizedBox.shrink());
      await tester.pump();
      await connection.close();
    },
  );
}

class _ChatApi implements ControlApi {
  final pane = PaneInfo.fromJson({
    'pane_id': '%7',
    'pane_title': 'Android chat',
    'state': 'busy',
    'detailed_state': 'working',
    'working_dir': '/tmp',
    'account': 'default',
    'backend': 'codex',
    'session_id': 'session-7',
    'tags': <String>[],
    'created_at': 1,
    'last_active': 1,
  });
  final records = <Map<String, dynamic>>[
    {
      'cursor': 1,
      'event_type': 'userMessage',
      'payload': {'text': 'First question'},
    },
    {
      'cursor': 2,
      'event_type': 'agentMessage',
      'payload': {'text': 'First answer'},
    },
  ];
  final streams = <StreamController<String>>[];
  bool liveAvailable = true;
  int get liveStarts => streams.length;

  void add(int cursor, String type, String text) {
    final record = {
      'cursor': cursor,
      'event_type': type,
      'payload': {'text': text},
    };
    records.add(record);
    if (streams.isNotEmpty && !streams.last.isClosed) {
      streams.last.add(jsonEncode({...record, 'source': 'transcript'}));
    }
  }

  Future<void> endLive() => streams.last.close();

  @override
  Future<CommandResult> command(
    List<String> arguments, {
    Duration timeout = const Duration(seconds: 45),
  }) async => CommandResult(
    arguments.first == 'transcript'
        ? jsonEncode({'records': records})
        : jsonEncode([
            {
              'pane_id': pane.id,
              'pane_title': pane.title,
              'state': pane.state,
              'detailed_state': pane.detailedState,
              'working_dir': pane.directory,
              'account': pane.account,
              'backend': pane.backend,
              'session_id': pane.sessionId,
              'tags': pane.tags,
              'created_at': pane.createdAt,
              'last_active': pane.lastActive,
            },
          ]),
    '',
    0,
  );

  @override
  Future<CommandStream> openStream(List<String> arguments) async {
    if (!liveAvailable) throw StateError('stream unavailable');
    final stream = StreamController<String>();
    streams.add(stream);
    return CommandStream(
      lines: stream.stream,
      errors: const Stream.empty(),
      done: stream.done.then((_) => 0),
      close: () {
        if (!stream.isClosed) stream.close();
      },
    );
  }

  @override
  Future<void> close() async {
    for (final stream in streams) {
      if (!stream.isClosed) await stream.close();
    }
  }
}
