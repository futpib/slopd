import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:slopd_control/control.dart';
import 'package:slopd_control/models.dart';
import 'package:slopd_control/pane_page.dart';
import 'package:slopd_control/ui.dart';

void main() {
  PaneInfo pane() => PaneInfo.fromJson({
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

  testWidgets('pane presents a conversation with collapsible agent work', (
    tester,
  ) async {
    final fake = _FakeControlApi(pane());
    final connection = ControlConnection(fake, () async => fake);

    await tester.pumpWidget(
      MaterialApp(
        theme: appTheme(Brightness.dark),
        home: PanePage(api: connection, pane: fake.pane),
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

  testWidgets('chat catches up after live loss, backgrounding, and reconnect', (
    tester,
  ) async {
    final fake = _FakeControlApi(pane());
    final connection = ControlConnection(fake, () async => fake);
    await tester.pumpWidget(
      MaterialApp(
        home: PanePage(api: connection, pane: fake.pane),
      ),
    );
    await tester.pump(const Duration(milliseconds: 100));
    expect(fake.liveStarts, 1);

    fake.addRecord(6, 'agentMessage', 'Live answer');
    await tester.pump();
    expect(find.text('Live answer'), findsOneWidget);

    fake.liveUnavailable = true;
    await fake.endLive();
    fake.transcript.removeWhere((record) => record['cursor'] == 5);
    fake.addRecord(7, 'agentMessage', 'Missed while offline');
    await tester.pump(const Duration(seconds: 3));
    await tester.pump();
    expect(fake.liveStarts, 1);
    expect(find.text('Missed while offline'), findsOneWidget);
    expect(find.text('Live answer'), findsOneWidget);
    expect(find.text('It now reads like a conversation.'), findsNothing);
    expect(find.textContaining('Live transcript failed'), findsNothing);

    fake.liveUnavailable = false;
    await tester.pump(const Duration(seconds: 3));
    expect(fake.liveStarts, 2);

    fake.transcript.removeWhere((record) => record['cursor'] == 7);
    await tester.pump(const Duration(seconds: 3));
    await tester.pump();
    expect(find.text('Missed while offline'), findsNothing);

    for (final state in [
      AppLifecycleState.inactive,
      AppLifecycleState.hidden,
      AppLifecycleState.paused,
    ]) {
      tester.binding.handleAppLifecycleStateChanged(state);
    }
    await tester.pump();
    fake.addRecord(8, 'agentMessage', 'Arrived in background');
    final startsWhilePaused = fake.liveStarts;
    await tester.pump(const Duration(seconds: 3));
    expect(fake.liveStarts, startsWhilePaused);
    expect(find.text('Arrived in background'), findsNothing);

    for (final state in [
      AppLifecycleState.hidden,
      AppLifecycleState.inactive,
      AppLifecycleState.resumed,
    ]) {
      tester.binding.handleAppLifecycleStateChanged(state);
    }
    await tester.pump(const Duration(milliseconds: 100));
    expect(find.text('Arrived in background'), findsOneWidget);

    await tester.enterText(
      find.widgetWithText(TextField, 'Message agent'),
      'Keep this draft',
    );
    await connection.reconnect();
    await tester.pump(const Duration(milliseconds: 100));
    expect(fake.liveStarts, greaterThan(startsWhilePaused));
    expect(find.text('Keep this draft'), findsOneWidget);

    await tester.pumpWidget(const SizedBox.shrink());
    await tester.pump();
    await connection.close();
  });

  testWidgets(
    'activity expansion stays with its records through chat updates',
    (tester) async {
      final fake = _FakeControlApi(pane());
      final connection = ControlConnection(fake, () async => fake);
      final viewState = PaneViewState();
      await tester.pumpWidget(
        MaterialApp(
          home: PanePage(
            api: connection,
            pane: fake.pane,
            viewState: viewState,
          ),
        ),
      );
      await tester.pumpAndSettle();

      await tester.tap(find.text('3 steps'));
      await tester.pumpAndSettle();
      expect(find.text('command output'), findsOneWidget);

      fake.transcript.insert(0, {
        'cursor': 0,
        'event_type': 'userMessage',
        'payload': {'text': 'Earlier question'},
      });
      await tester.tap(find.byTooltip('Refresh chat'));
      await tester.pumpAndSettle();
      expect(find.text('command output'), findsOneWidget);

      for (var cursor = 6; cursor <= 45; cursor++) {
        fake.transcript.add({
          'cursor': cursor,
          'event_type': 'userMessage',
          'payload': {'text': 'Later question $cursor'},
        });
      }
      await tester.tap(find.byTooltip('Refresh chat'));
      await tester.pumpAndSettle();
      await tester.drag(find.byType(CustomScrollView), const Offset(0, 10000));
      await tester.pumpAndSettle();
      expect(find.text('command output'), findsOneWidget);

      await tester.pumpWidget(const SizedBox.shrink());
      await tester.pumpWidget(
        MaterialApp(
          home: PanePage(
            api: connection,
            pane: fake.pane,
            viewState: viewState,
          ),
        ),
      );
      await tester.pumpAndSettle();
      expect(find.text('command output'), findsOneWidget);

      await tester.tap(find.text('3 steps'));
      await tester.pumpAndSettle();
      expect(find.text('command output'), findsNothing);
      await connection.reconnect();
      await tester.pumpAndSettle();
      expect(find.text('command output'), findsNothing);

      await tester.pumpWidget(const SizedBox.shrink());
      await tester.pump();
      await connection.close();
    },
  );

  testWidgets('new chat activity keeps the reader position until near bottom', (
    tester,
  ) async {
    final fake = _FakeControlApi(pane());
    final viewState = PaneViewState();
    fake.transcript.clear();
    for (var i = 1; i <= 40; i++) {
      fake.transcript.add({
        'cursor': i,
        'event_type': i.isOdd ? 'userMessage' : 'agentMessage',
        'payload': {'text': 'Message $i'},
      });
    }
    final connection = ControlConnection(fake, () async => fake);
    await tester.pumpWidget(
      MaterialApp(
        home: PanePage(api: connection, pane: fake.pane, viewState: viewState),
      ),
    );
    await tester.pumpAndSettle();
    var scroll = tester
        .widget<CustomScrollView>(find.byType(CustomScrollView))
        .controller!;

    await tester.drag(find.byType(CustomScrollView), const Offset(0, 500));
    await tester.pumpAndSettle();
    final readingOffset = scroll.offset;
    expect(scroll.position.maxScrollExtent - readingOffset, greaterThan(160));

    fake.addRecord(41, 'userMessage', 'Message 41');
    await tester.pumpAndSettle();
    expect(scroll.offset, closeTo(readingOffset, 1));

    for (final state in [
      AppLifecycleState.inactive,
      AppLifecycleState.hidden,
      AppLifecycleState.paused,
      AppLifecycleState.hidden,
      AppLifecycleState.inactive,
      AppLifecycleState.resumed,
    ]) {
      tester.binding.handleAppLifecycleStateChanged(state);
    }
    await tester.pumpAndSettle();
    expect(scroll.offset, closeTo(readingOffset, 1));
    await connection.reconnect();
    await tester.pumpAndSettle();
    expect(scroll.offset, closeTo(readingOffset, 1));

    await tester.pumpWidget(const SizedBox.shrink());
    await tester.pumpWidget(
      MaterialApp(
        home: PanePage(api: connection, pane: fake.pane, viewState: viewState),
      ),
    );
    await tester.pumpAndSettle();
    scroll = tester
        .widget<CustomScrollView>(find.byType(CustomScrollView))
        .controller!;
    expect(scroll.offset, closeTo(readingOffset, 1));

    await tester.drag(find.byType(CustomScrollView), const Offset(0, -10000));
    await tester.pumpAndSettle();
    fake.addRecord(42, 'agentMessage', 'Message 42');
    await tester.pumpAndSettle();
    expect(scroll.offset, closeTo(scroll.position.maxScrollExtent, 1));

    await tester.pumpWidget(const SizedBox.shrink());
    await tester.pump();
    await connection.close();
  });

  testWidgets('loading earlier messages keeps the current reading place', (
    tester,
  ) async {
    final fake = _FakeControlApi(pane());
    final viewState = PaneViewState();
    fake.transcript.clear();
    for (var cursor = 1; cursor <= 320; cursor++) {
      fake.transcript.add({
        'cursor': cursor,
        'event_type': 'userMessage',
        'payload': {'text': 'Message $cursor'},
      });
    }
    final connection = ControlConnection(fake, () async => fake);
    await tester.pumpWidget(
      MaterialApp(
        home: PanePage(api: connection, pane: fake.pane, viewState: viewState),
      ),
    );
    await tester.pumpAndSettle();
    final scroll = tester
        .widget<CustomScrollView>(find.byType(CustomScrollView))
        .controller!;
    await tester.drag(find.byType(CustomScrollView), const Offset(0, 100000));
    await tester.pumpAndSettle();
    expect(find.text('Message 21'), findsOneWidget);
    final readingOffset = scroll.offset;

    await tester.tap(find.text('Earlier messages'));
    await tester.pumpAndSettle();
    expect(
      find.text('Message 21'),
      findsOneWidget,
      reason: 'offset ${scroll.offset}, max ${scroll.position.maxScrollExtent}',
    );
    expect(scroll.offset, closeTo(readingOffset, 1));

    await tester.pumpWidget(const SizedBox.shrink());
    await tester.pumpWidget(
      MaterialApp(
        home: PanePage(api: connection, pane: fake.pane, viewState: viewState),
      ),
    );
    await tester.pumpAndSettle();
    final restored = tester
        .widget<CustomScrollView>(find.byType(CustomScrollView))
        .controller!;
    expect(find.text('Message 21'), findsOneWidget);
    expect(restored.offset, closeTo(readingOffset, 1));

    await tester.pumpWidget(const SizedBox.shrink());
    await tester.pump();
    await connection.close();
  });
}

class _FakeControlApi implements ControlApi {
  _FakeControlApi(this.pane);

  final PaneInfo pane;
  final commands = <List<String>>[];
  final _streams = <StreamController<String>>[];
  bool liveUnavailable = false;
  final transcript = <Map<String, dynamic>>[
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
  ];

  int get liveStarts => _streams.length;

  void addRecord(int cursor, String type, String text) {
    final record = {
      'cursor': cursor,
      'event_type': type,
      'payload': {'text': text},
    };
    transcript.add(record);
    if (_streams.isNotEmpty && !_streams.last.isClosed) {
      _streams.last.add(jsonEncode({...record, 'source': 'transcript'}));
    }
  }

  Future<void> endLive() => _streams.last.close();

  @override
  Future<CommandResult> command(
    List<String> arguments, {
    Duration timeout = const Duration(seconds: 45),
  }) async {
    commands.add(arguments);
    if (arguments.first == 'transcript') {
      final beforeIndex = arguments.indexOf('--before');
      final limitIndex = arguments.indexOf('--limit');
      final before = beforeIndex < 0
          ? null
          : int.parse(arguments[beforeIndex + 1]);
      final limit = int.parse(arguments[limitIndex + 1]);
      final matching =
          transcript
              .where(
                (record) =>
                    before == null || (record['cursor'] as int) < before,
              )
              .toList()
            ..sort(
              (first, second) =>
                  (first['cursor'] as int).compareTo(second['cursor'] as int),
            );
      final page = matching.length > limit
          ? matching.sublist(matching.length - limit)
          : matching;
      return CommandResult(jsonEncode({'records': page}), '', 0);
    }
    if (arguments.first == 'ps') {
      return CommandResult(jsonEncode([_paneJson(pane)]), '', 0);
    }
    return const CommandResult('', '', 0);
  }

  @override
  Future<CommandStream> openStream(List<String> arguments) async {
    if (liveUnavailable) throw StateError('feed offline');
    final live = StreamController<String>();
    _streams.add(live);
    return CommandStream(
      lines: live.stream,
      errors: const Stream.empty(),
      done: live.done.then((_) => 0),
      close: () {
        if (!live.isClosed) live.close();
      },
    );
  }

  @override
  Future<void> close() async {
    for (final live in _streams) {
      if (!live.isClosed) await live.close();
    }
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
