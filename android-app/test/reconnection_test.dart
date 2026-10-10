import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:slopd_control/control.dart';

class TestControl implements ControlApi {
  bool lost = false;
  int closes = 0;
  final calls = <List<String>>[];
  Future<CommandResult> Function()? respond;

  @override
  Future<CommandResult> command(
    List<String> arguments, {
    Duration timeout = const Duration(seconds: 45),
  }) async {
    calls.add(arguments);
    if (lost) throw const ConnectionLost();
    if (respond != null) return respond!();
    return const CommandResult('', '', 0);
  }

  @override
  Future<void> close() async => closes++;
}

void main() {
  test(
    'lost commands are never replayed and reconnects share a session',
    () async {
      final old = TestControl()..lost = true;
      final next = TestControl();
      final connecting = Completer<ControlApi>();
      var attempts = 0;
      final api = ControlConnection(old, () {
        attempts++;
        return connecting.future;
      });

      await expectLater(
        api.command(['send', '%1', 'hello']),
        throwsA(isA<ConnectionLost>()),
      );
      expect(api.disconnected, isTrue);
      final first = api.reconnect();
      final second = api.reconnect();
      expect(identical(first, second), isTrue);
      connecting.complete(next);
      await first;

      expect(attempts, 1);
      expect(old.closes, 1);
      expect(next.calls, isEmpty);
      await api.command(['ps', '--json']);
      expect(next.calls.single, ['ps', '--json']);
      await api.close();
      expect(next.closes, 1);
      api.dispose();
    },
  );

  test(
    'late failures from an old session do not break its replacement',
    () async {
      final pending = Completer<CommandResult>();
      final old = TestControl()..respond = () => pending.future;
      final api = ControlConnection(old, () async => TestControl());
      final command = api.command(['ps']);
      final failed = expectLater(command, throwsA(isA<ConnectionLost>()));
      await api.reconnect();
      pending.completeError(const ConnectionLost());
      await failed;
      expect(api.disconnected, isFalse);
      await api.close();
      api.dispose();
    },
  );

  test('daemon errors do not mark SSH disconnected', () async {
    final old = TestControl()
      ..respond = () async => throw StateError('Daemon is stopped');
    final api = ControlConnection(old, () async => TestControl());
    await expectLater(api.command(['ps']), throwsStateError);
    expect(api.disconnected, isFalse);
    await api.close();
    api.dispose();
  });

  test('shell arguments are quoted independently', () {
    expect(shellQuote("a'b"), "'a'\"'\"'b'");
    expect(shellQuote('! bash -c echo'), "'! bash -c echo'");
  });
}
