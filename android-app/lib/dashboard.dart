import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';

import 'control.dart';
import 'ui.dart';

class PaneInfo {
  const PaneInfo({
    required this.id,
    required this.title,
    required this.state,
    required this.detailedState,
    required this.directory,
    required this.account,
    required this.backend,
    required this.sessionId,
    required this.tags,
    required this.lastActive,
  });

  final String id,
      title,
      state,
      detailedState,
      directory,
      account,
      backend,
      sessionId;
  final List<String> tags;
  final int lastActive;

  factory PaneInfo.fromJson(Map<String, dynamic> value) => PaneInfo(
    id: value['pane_id'] as String,
    title: (value['pane_title'] as String?)?.trim().isNotEmpty == true
        ? value['pane_title'] as String
        : value['pane_id'] as String,
    state: value['state'] as String? ?? 'unknown',
    detailedState: value['detailed_state'] as String? ?? 'unknown',
    directory: value['working_dir'] as String? ?? '',
    account: value['account'] as String? ?? '',
    backend: value['backend'] as String? ?? '',
    sessionId: value['session_id'] as String? ?? '',
    tags: (value['tags'] as List? ?? const []).cast<String>(),
    lastActive: value['last_active'] as int? ?? 0,
  );
}

class TranscriptEntry {
  const TranscriptEntry({
    required this.type,
    required this.payload,
    required this.cursor,
  });

  final String type;
  final dynamic payload;
  final int cursor;

  factory TranscriptEntry.fromJson(Map<String, dynamic> value) =>
      TranscriptEntry(
        type: value['event_type'] as String? ?? 'record',
        payload: value['payload'],
        cursor: value['cursor'] as int? ?? 0,
      );

  String get text {
    final value = payload;
    if (value is String) return value;
    if (value is Map) {
      for (final key in const ['text', 'content', 'message', 'output']) {
        final candidate = value[key];
        if (candidate is String && candidate.isNotEmpty) return candidate;
      }
    }
    return const JsonEncoder.withIndent('  ').convert(value);
  }
}

class Dashboard extends StatefulWidget {
  const Dashboard({
    super.key,
    required this.api,
    required this.name,
    required this.connection,
  });

  final ControlConnection api;
  final String name, connection;

  @override
  State<Dashboard> createState() => _DashboardState();
}

class _DashboardState extends State<Dashboard> with WidgetsBindingObserver {
  List<PaneInfo> panes = const [];
  String status = '', error = '';
  bool loading = false, refreshing = false;
  Timer? timer;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    widget.api.addListener(connectionChanged);
    refresh();
    timer = Timer.periodic(
      const Duration(seconds: 3),
      (_) => refresh(quiet: true),
    );
  }

  @override
  void dispose() {
    timer?.cancel();
    widget.api.removeListener(connectionChanged);
    WidgetsBinding.instance.removeObserver(this);
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (state == AppLifecycleState.resumed) refresh();
  }

  void connectionChanged() {
    if (mounted) setState(() {});
  }

  Future<void> refresh({bool quiet = false}) async {
    if (refreshing || widget.api.disconnected || widget.api.reconnecting) {
      return;
    }
    refreshing = true;
    if (!quiet && mounted) setState(() => loading = true);
    try {
      final results = await Future.wait([
        widget.api.command(['ps', '--json']),
        widget.api.command(['status']),
      ]);
      final values =
          (jsonDecode(results[0].stdout) as List)
              .map((value) => PaneInfo.fromJson(value as Map<String, dynamic>))
              .toList()
            ..sort((a, b) => b.lastActive.compareTo(a.lastActive));
      if (mounted) {
        setState(() {
          panes = values;
          status = results[1].stdout.trim();
          error = '';
        });
      }
    } on ConnectionLost {
      // The connection banner presents recovery.
    } catch (exception) {
      if (!quiet && mounted) setState(() => error = '$exception');
    } finally {
      refreshing = false;
      if (!quiet && mounted) setState(() => loading = false);
    }
  }

  Future<void> reconnect() async {
    setState(() => error = '');
    try {
      await widget.api.reconnect();
      await refresh();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> runPane() async {
    final directory = TextEditingController();
    final account = TextEditingController();
    String? backend;
    final request = await showDialog<List<String>>(
      context: context,
      builder: (context) => StatefulBuilder(
        builder: (context, setDialogState) => AlertDialog(
          title: const Text('Start agent'),
          content: SingleChildScrollView(
            child: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                TextField(
                  controller: directory,
                  decoration: const InputDecoration(
                    labelText: 'Working directory (optional)',
                    hintText: '~/code/project',
                  ),
                ),
                const SizedBox(height: 16),
                TextField(
                  controller: account,
                  decoration: const InputDecoration(
                    labelText: 'Account (optional)',
                  ),
                ),
                const SizedBox(height: 16),
                DropdownButtonFormField<String?>(
                  initialValue: backend,
                  decoration: const InputDecoration(labelText: 'Backend'),
                  items: const [
                    DropdownMenuItem(
                      value: null,
                      child: Text('Account default'),
                    ),
                    DropdownMenuItem(value: 'claude', child: Text('Claude')),
                    DropdownMenuItem(value: 'codex', child: Text('Codex')),
                    DropdownMenuItem(
                      value: 'opencode',
                      child: Text('OpenCode'),
                    ),
                    DropdownMenuItem(value: 'grok', child: Text('Grok')),
                  ],
                  onChanged: (value) => setDialogState(() => backend = value),
                ),
              ],
            ),
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context),
              child: const Text('Cancel'),
            ),
            FilledButton(
              onPressed: () {
                Navigator.pop(context, [
                  'run',
                  '--no-wait',
                  if (directory.text.trim().isNotEmpty) ...[
                    '--start-directory',
                    directory.text.trim(),
                  ],
                  if (account.text.trim().isNotEmpty) ...[
                    '--account',
                    account.text.trim(),
                  ],
                  if (backend != null) ...['--backend', backend!],
                ]);
              },
              child: const Text('Start'),
            ),
          ],
        ),
      ),
    );
    directory.dispose();
    account.dispose();
    if (request == null) return;
    setState(() {
      loading = true;
      error = '';
    });
    try {
      final result = await widget.api.command(request);
      await refresh(quiet: true);
      if (mounted && result.stdout.trim().isNotEmpty) {
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(content: Text('Started ${result.stdout.trim()}')),
        );
      }
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    } finally {
      if (mounted) setState(() => loading = false);
    }
  }

  Future<void> kill(PaneInfo pane) async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Kill ${pane.id}?'),
        content: Text(pane.title),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Kill'),
          ),
        ],
      ),
    );
    if (confirmed != true) return;
    try {
      await widget.api.command(['kill', pane.id]);
      await refresh();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> interrupt(PaneInfo pane) async {
    try {
      await widget.api.command(['interrupt', pane.id]);
      await refresh();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Color stateColor(BuildContext context, String state) => switch (state) {
    'ready' => Colors.green,
    'busy' => Theme.of(context).colorScheme.primary,
    'error' || 'dead' => Theme.of(context).colorScheme.error,
    _ => Theme.of(context).colorScheme.onSurfaceVariant,
  };

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(
      title: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(widget.name),
          Text(
            widget.connection,
            style: Theme.of(context).textTheme.labelSmall,
          ),
        ],
      ),
      actions: [
        IconButton(
          tooltip: 'Reconnect',
          onPressed: widget.api.reconnecting ? null : reconnect,
          icon: const Icon(Icons.link),
        ),
        IconButton(
          tooltip: 'Refresh',
          onPressed: loading ? null : refresh,
          icon: const Icon(Icons.refresh),
        ),
      ],
    ),
    floatingActionButton: FloatingActionButton.extended(
      onPressed: loading || widget.api.disconnected ? null : runPane,
      icon: const Icon(Icons.add),
      label: const Text('Start agent'),
    ),
    body: RefreshIndicator(
      onRefresh: refresh,
      child: ListView(
        physics: const AlwaysScrollableScrollPhysics(),
        padding: const EdgeInsets.fromLTRB(20, 8, 20, 100),
        children: [
          if (widget.api.disconnected || widget.api.reconnecting)
            Card(
              color: Theme.of(context).colorScheme.errorContainer,
              child: ListTile(
                leading: const Icon(Icons.link_off),
                title: Text(
                  widget.api.reconnecting ? 'Reconnecting…' : 'Connection lost',
                ),
                trailing: widget.api.reconnecting
                    ? const SizedBox.square(
                        dimension: 22,
                        child: CircularProgressIndicator(strokeWidth: 2),
                      )
                    : TextButton(
                        onPressed: reconnect,
                        child: const Text('Reconnect'),
                      ),
              ),
            ),
          if (error.isNotEmpty) ErrorBox(error),
          if (status.isNotEmpty)
            Padding(
              padding: const EdgeInsets.symmetric(vertical: 12),
              child: Text(status, style: mono.copyWith(fontSize: 11)),
            ),
          SectionHeading('Agents · ${panes.length}'),
          if (loading && panes.isEmpty)
            const Center(
              child: Padding(
                padding: EdgeInsets.all(48),
                child: CircularProgressIndicator(),
              ),
            )
          else if (panes.isEmpty)
            const EmptyState(
              Icons.smart_toy_outlined,
              'No managed agents',
              'Start an agent on this server to manage it here.',
            ),
          for (final pane in panes)
            Card(
              child: ListTile(
                contentPadding: const EdgeInsets.fromLTRB(16, 10, 8, 10),
                leading: Icon(
                  pane.state == 'busy'
                      ? Icons.pending_outlined
                      : Icons.smart_toy_outlined,
                  color: stateColor(context, pane.state),
                ),
                title: Text(
                  pane.title,
                  maxLines: 2,
                  overflow: TextOverflow.ellipsis,
                ),
                subtitle: Text(
                  '${pane.id} · ${pane.backend}/${pane.account}\n${pane.detailedState}${pane.directory.isEmpty ? '' : ' · ${pane.directory}'}',
                  maxLines: 3,
                  overflow: TextOverflow.ellipsis,
                ),
                onTap: () async {
                  await Navigator.of(context).push<void>(
                    MaterialPageRoute(
                      builder: (_) => PanePage(api: widget.api, pane: pane),
                    ),
                  );
                  await refresh();
                },
                trailing: PopupMenuButton<String>(
                  onSelected: (value) =>
                      value == 'kill' ? kill(pane) : interrupt(pane),
                  itemBuilder: (_) => const [
                    PopupMenuItem(value: 'interrupt', child: Text('Interrupt')),
                    PopupMenuItem(value: 'kill', child: Text('Kill')),
                  ],
                ),
              ),
            ),
        ],
      ),
    ),
  );
}

class PanePage extends StatefulWidget {
  const PanePage({super.key, required this.api, required this.pane});
  final ControlConnection api;
  final PaneInfo pane;

  @override
  State<PanePage> createState() => _PanePageState();
}

class _PanePageState extends State<PanePage> {
  final prompt = TextEditingController();
  List<TranscriptEntry> records = const [];
  String error = '';
  bool loading = false,
      refreshing = false,
      sending = false,
      interruptFirst = false;
  Timer? timer;

  @override
  void initState() {
    super.initState();
    refresh();
    timer = Timer.periodic(
      const Duration(seconds: 3),
      (_) => refresh(quiet: true),
    );
  }

  @override
  void dispose() {
    timer?.cancel();
    prompt.dispose();
    super.dispose();
  }

  Future<void> refresh({bool quiet = false}) async {
    if (refreshing || widget.api.disconnected) return;
    refreshing = true;
    if (!quiet) setState(() => loading = true);
    try {
      final result = await widget.api.command([
        'transcript',
        '--limit',
        '100',
        widget.pane.id,
      ]);
      final value = jsonDecode(result.stdout) as Map<String, dynamic>;
      final next = (value['records'] as List? ?? const [])
          .map(
            (record) =>
                TranscriptEntry.fromJson(record as Map<String, dynamic>),
          )
          .toList();
      if (mounted) {
        setState(() {
          records = next;
          error = '';
        });
      }
    } on ConnectionLost {
      if (mounted) setState(() {});
    } catch (exception) {
      if (!quiet && mounted) setState(() => error = '$exception');
    } finally {
      refreshing = false;
      if (!quiet && mounted) setState(() => loading = false);
    }
  }

  Future<void> send() async {
    final text = prompt.text;
    if (text.trim().isEmpty || sending) return;
    setState(() {
      sending = true;
      error = '';
    });
    try {
      await widget.api.command([
        'send',
        if (interruptFirst) '--interrupt',
        widget.pane.id,
        text,
      ], timeout: const Duration(seconds: 75));
      prompt.clear();
      await refresh(quiet: true);
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    } finally {
      if (mounted) setState(() => sending = false);
    }
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(
      title: Text(widget.pane.id),
      actions: [
        IconButton(
          tooltip: 'Refresh transcript',
          onPressed: loading ? null : refresh,
          icon: const Icon(Icons.refresh),
        ),
      ],
    ),
    body: Column(
      children: [
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 8, 16, 0),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Text(
                widget.pane.title,
                style: Theme.of(context).textTheme.titleMedium,
              ),
              const SizedBox(height: 4),
              Text(
                '${widget.pane.backend}/${widget.pane.account} · ${widget.pane.detailedState}\n${widget.pane.directory}',
                style: Theme.of(context).textTheme.bodySmall,
              ),
              if (error.isNotEmpty) ...[
                const SizedBox(height: 8),
                ErrorBox(error),
              ],
            ],
          ),
        ),
        Expanded(
          child: loading && records.isEmpty
              ? const Center(child: CircularProgressIndicator())
              : records.isEmpty
              ? const EmptyState(
                  Icons.notes,
                  'No transcript records',
                  'Records appear after the agent receives input.',
                )
              : ListView.builder(
                  padding: const EdgeInsets.all(16),
                  itemCount: records.length,
                  itemBuilder: (context, index) {
                    final record = records[index];
                    return Card(
                      child: Padding(
                        padding: const EdgeInsets.all(14),
                        child: Column(
                          crossAxisAlignment: CrossAxisAlignment.start,
                          children: [
                            Text(
                              record.type,
                              style: Theme.of(context).textTheme.labelMedium,
                            ),
                            const SizedBox(height: 8),
                            SelectableText(record.text, style: mono),
                          ],
                        ),
                      ),
                    );
                  },
                ),
        ),
        SafeArea(
          top: false,
          child: Material(
            color: Theme.of(context).colorScheme.surfaceContainer,
            child: Padding(
              padding: const EdgeInsets.fromLTRB(12, 10, 8, 10),
              child: Row(
                crossAxisAlignment: CrossAxisAlignment.end,
                children: [
                  Expanded(
                    child: TextField(
                      controller: prompt,
                      minLines: 1,
                      maxLines: 5,
                      enabled: !sending && !widget.api.disconnected,
                      decoration: const InputDecoration(
                        labelText: 'Prompt or local command',
                        hintText: 'Continue, or ! command',
                      ),
                    ),
                  ),
                  const SizedBox(width: 8),
                  Column(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      Tooltip(
                        message: 'Interrupt before sending',
                        child: Checkbox(
                          value: interruptFirst,
                          onChanged: sending
                              ? null
                              : (value) => setState(
                                  () => interruptFirst = value ?? false,
                                ),
                        ),
                      ),
                      IconButton.filled(
                        tooltip: 'Send',
                        onPressed: sending || widget.api.disconnected
                            ? null
                            : send,
                        icon: sending
                            ? const SizedBox.square(
                                dimension: 20,
                                child: CircularProgressIndicator(
                                  strokeWidth: 2,
                                ),
                              )
                            : const Icon(Icons.send),
                      ),
                    ],
                  ),
                ],
              ),
            ),
          ),
        ),
      ],
    ),
  );
}
