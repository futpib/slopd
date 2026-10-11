class LaunchOptions {
  const LaunchOptions({
    this.directory = '',
    this.account = '',
    this.backend,
    this.environment = const [],
    this.environmentFiles = const [],
    this.extraArguments = const [],
    this.noWait = false,
    this.readyTimeout = 30,
  });

  final String directory, account;
  final String? backend;
  final List<String> environment, environmentFiles, extraArguments;
  final bool noWait;
  final int readyTimeout;

  List<String> runArguments() => _arguments('run');
  List<String> forkArguments(String paneId) => _arguments('fork', paneId);

  List<String> _arguments(String command, [String? paneId]) => [
    command,
    ?paneId,
    if (noWait) '--no-wait',
    if (!noWait) ...['--ready-timeout', '$readyTimeout'],
    if (directory.isNotEmpty) ...['--start-directory', directory],
    if (command == 'run' && account.isNotEmpty) ...['--account', account],
    if (command == 'run' && backend != null) ...['--backend', backend!],
    for (final value in environment) ...['--env', value],
    for (final value in environmentFiles) ...['--env-file', value],
    if (extraArguments.isNotEmpty) ...['--', ...extraArguments],
  ];
}

class SendOptions {
  const SendOptions({
    required this.target,
    required this.prompt,
    this.filters = const [],
    this.selection = 'one',
    this.timeout = 60,
    this.interrupt = false,
  });

  final String target, prompt, selection;
  final List<String> filters;
  final int timeout;
  final bool interrupt;

  List<String> arguments() => [
    'send',
    if (interrupt) '--interrupt',
    for (final filter in filters) ...['--filter', filter],
    '--select',
    selection,
    '--timeout',
    '$timeout',
    target,
    prompt,
  ];
}

class EventOptions {
  const EventOptions({
    this.wait = false,
    this.hooks = const [],
    this.events = const [],
    this.transcripts = const [],
    this.paneId = '',
    this.sessionId = '',
    this.where = const [],
    this.until = const [],
    this.timeout = 60,
    this.noSnapshot = false,
  });

  final bool wait, noSnapshot;
  final List<String> hooks, events, transcripts, where, until;
  final String paneId, sessionId;
  final int timeout;

  List<String> arguments() => [
    wait ? 'wait' : 'listen',
    for (final value in hooks) ...['--hook', value],
    for (final value in events) ...['--event', value],
    for (final value in transcripts) ...['--transcript', value],
    if (paneId.isNotEmpty) ...['--pane-id', paneId],
    if (sessionId.isNotEmpty) ...['--session-id', sessionId],
    for (final value in where) ...['--where', value],
    if (wait) ...[
      for (final value in until) ...['--until', value],
      if (noSnapshot) '--no-snapshot',
      '--timeout',
      '$timeout',
    ],
  ];
}

List<String> nonEmptyLines(String value) => value
    .split('\n')
    .map((line) => line.trim())
    .where((line) => line.isNotEmpty)
    .toList();
