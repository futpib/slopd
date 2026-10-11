import 'package:flutter_test/flutter_test.dart';
import 'package:slopd_control/operations.dart';

void main() {
  test('run waits for readiness and preserves each remote argument', () {
    const options = LaunchOptions(
      directory: '/srv/my project',
      account: 'work',
      backend: 'codex',
      environment: ['TOKEN=a b'],
      environmentFiles: ['/srv/env file'],
      extraArguments: ['--model', 'gpt 6'],
      readyTimeout: 45,
    );
    expect(options.runArguments(), [
      'run',
      '--ready-timeout',
      '45',
      '--start-directory',
      '/srv/my project',
      '--account',
      'work',
      '--backend',
      'codex',
      '--env',
      'TOKEN=a b',
      '--env-file',
      '/srv/env file',
      '--',
      '--model',
      'gpt 6',
    ]);
    expect(options.runArguments(), isNot(contains('--no-wait')));
  });

  test('fork and filtered send expose the complete CLI controls', () {
    const launch = LaunchOptions(noWait: true, environment: ['A=1']);
    expect(launch.forkArguments('%7'), [
      'fork',
      '%7',
      '--no-wait',
      '--env',
      'A=1',
    ]);

    const send = SendOptions(
      target: 'tag=worker',
      prompt: 'continue',
      filters: ['backend=codex', 'account=work'],
      selection: 'all',
      timeout: 75,
      interrupt: true,
    );
    expect(send.arguments(), [
      'send',
      '--interrupt',
      '--filter',
      'backend=codex',
      '--filter',
      'account=work',
      '--select',
      'all',
      '--timeout',
      '75',
      'tag=worker',
      'continue',
    ]);
  });

  test('listen and wait builders retain event predicates', () {
    const listen = EventOptions(
      events: ['StateChange'],
      paneId: '%3',
      where: ['state=ready'],
    );
    expect(listen.arguments(), [
      'listen',
      '--event',
      'StateChange',
      '--pane-id',
      '%3',
      '--where',
      'state=ready',
    ]);

    const wait = EventOptions(
      wait: true,
      sessionId: 'session',
      until: ['state=ready'],
      timeout: 0,
      noSnapshot: true,
    );
    expect(wait.arguments(), [
      'wait',
      '--session-id',
      'session',
      '--until',
      'state=ready',
      '--no-snapshot',
      '--timeout',
      '0',
    ]);
  });
}
