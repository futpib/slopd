import 'dart:convert';

import 'package:flutter/material.dart';

import 'control.dart';
import 'models.dart';
import 'operations.dart';
import 'ui.dart';

class GraveyardPage extends StatefulWidget {
  const GraveyardPage({super.key, required this.api});

  final ControlConnection api;

  @override
  State<GraveyardPage> createState() => _GraveyardPageState();
}

class _GraveyardPageState extends State<GraveyardPage>
    with WidgetsBindingObserver {
  final search = TextEditingController();
  List<GraveEntry> entries = const [];
  String error = '';
  bool loading = false, unrevivedOnly = false;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    widget.api.addListener(_connectionChanged);
    search.addListener(_searchChanged);
    refresh();
  }

  @override
  void dispose() {
    widget.api.removeListener(_connectionChanged);
    WidgetsBinding.instance.removeObserver(this);
    search.removeListener(_searchChanged);
    search.dispose();
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (state == AppLifecycleState.resumed) refresh();
  }

  void _connectionChanged() {
    if (mounted) setState(() {});
  }

  void _searchChanged() => setState(() {});

  Future<void> refresh({bool force = false}) async {
    if ((!force && loading) ||
        widget.api.disconnected ||
        widget.api.reconnecting) {
      return;
    }
    setState(() => loading = true);
    try {
      final result = await widget.api.command([
        'graveyard',
        '--limit',
        '200',
        '--json',
      ]);
      final values = (jsonDecode(result.stdout) as List)
          .map((value) => GraveEntry.fromJson(value as Map<String, dynamic>))
          .toList();
      if (mounted) {
        setState(() {
          entries = values;
          error = '';
        });
      }
    } on ConnectionLost {
      if (mounted) setState(() {});
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    } finally {
      if (mounted) setState(() => loading = false);
    }
  }

  Future<void> reconnect() async {
    try {
      await widget.api.reconnect();
      await refresh(force: true);
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> revive(GraveEntry entry) async {
    final environment = TextEditingController();
    final files = TextEditingController();
    String? validation;
    final values = await showDialog<(List<String>, List<String>)>(
      context: context,
      builder: (context) => StatefulBuilder(
        builder: (context, setDialogState) => AlertDialog(
          title: Text('Revive ${entry.pane.id}?'),
          content: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              Text(entry.pane.title),
              const SizedBox(height: 16),
              TextField(
                controller: environment,
                minLines: 2,
                maxLines: 5,
                decoration: const InputDecoration(
                  labelText: 'Environment (one KEY=VALUE per line)',
                ),
              ),
              const SizedBox(height: 16),
              TextField(
                controller: files,
                minLines: 1,
                maxLines: 4,
                decoration: const InputDecoration(
                  labelText: 'Environment files (one path per line)',
                ),
              ),
              if (validation != null)
                Padding(
                  padding: const EdgeInsets.only(top: 12),
                  child: Text(
                    validation!,
                    style: TextStyle(
                      color: Theme.of(context).colorScheme.error,
                    ),
                  ),
                ),
            ],
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context),
              child: const Text('Cancel'),
            ),
            FilledButton(
              onPressed: () {
                final env = nonEmptyLines(environment.text);
                if (env.any((value) => !value.contains('='))) {
                  setDialogState(
                    () =>
                        validation = 'Every environment entry needs KEY=VALUE.',
                  );
                  return;
                }
                Navigator.pop(context, (env, nonEmptyLines(files.text)));
              },
              child: const Text('Revive'),
            ),
          ],
        ),
      ),
    );
    environment.dispose();
    files.dispose();
    if (values == null) return;
    setState(() => loading = true);
    try {
      final result = await widget.api.command([
        'revive',
        entry.id,
        for (final value in values.$1) ...['--env', value],
        for (final value in values.$2) ...['--env-file', value],
      ], timeout: const Duration(seconds: 75));
      await refresh(force: true);
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(content: Text('Revived as ${result.stdout.trim()}')),
        );
      }
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    } finally {
      if (mounted) setState(() => loading = false);
    }
  }

  List<GraveEntry> get visible {
    final query = search.text.trim().toLowerCase();
    return entries.where((entry) {
      if (unrevivedOnly && entry.revived) return false;
      if (query.isEmpty) return true;
      return [
        entry.id,
        entry.pane.id,
        entry.pane.title,
        entry.pane.sessionId,
        entry.pane.account,
        entry.pane.backend,
        entry.cause,
        ...entry.pane.tags,
      ].any((value) => value.toLowerCase().contains(query));
    }).toList();
  }

  @override
  Widget build(BuildContext context) {
    final shown = visible;
    return Scaffold(
      appBar: AppBar(
        title: const Text('Lifecycle graveyard'),
        actions: [
          IconButton(
            tooltip: 'Refresh',
            onPressed: loading ? null : refresh,
            icon: const Icon(Icons.refresh),
          ),
        ],
      ),
      body: Column(
        children: [
          if (widget.api.disconnected || widget.api.reconnecting)
            MaterialBanner(
              content: Text(
                widget.api.reconnecting ? 'Reconnecting…' : 'Connection lost',
              ),
              actions: [
                if (!widget.api.reconnecting)
                  TextButton(
                    onPressed: reconnect,
                    child: const Text('Reconnect'),
                  ),
              ],
            ),
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 12, 16, 8),
            child: Column(
              children: [
                TextField(
                  controller: search,
                  decoration: const InputDecoration(
                    labelText: 'Search graveyard',
                    prefixIcon: Icon(Icons.search),
                  ),
                ),
                SwitchListTile(
                  contentPadding: EdgeInsets.zero,
                  title: const Text('Only panes not yet revived'),
                  value: unrevivedOnly,
                  onChanged: (value) => setState(() => unrevivedOnly = value),
                ),
                if (error.isNotEmpty) ErrorBox(error),
              ],
            ),
          ),
          Expanded(
            child: loading && entries.isEmpty
                ? const Center(child: CircularProgressIndicator())
                : shown.isEmpty
                ? const EmptyState(
                    Icons.history,
                    'No matching lifecycle records',
                    'Destroyed managed panes appear here when retained by the server.',
                  )
                : ListView.builder(
                    padding: const EdgeInsets.fromLTRB(16, 8, 16, 24),
                    itemCount: shown.length,
                    itemBuilder: (context, index) {
                      final entry = shown[index];
                      return Card(
                        child: Padding(
                          padding: const EdgeInsets.all(16),
                          child: Column(
                            crossAxisAlignment: CrossAxisAlignment.start,
                            children: [
                              Text(
                                entry.pane.title,
                                style: Theme.of(context).textTheme.titleMedium,
                              ),
                              const SizedBox(height: 6),
                              SelectableText(
                                '${entry.pane.id} · ${entry.pane.backend}/${entry.pane.account}\n'
                                '${entry.cause} · destroyed ${formatAge(entry.destroyedAt)}\n'
                                '${entry.id}',
                                style: mono.copyWith(fontSize: 11),
                              ),
                              if (entry.pane.tags.isNotEmpty) ...[
                                const SizedBox(height: 8),
                                Wrap(
                                  spacing: 6,
                                  children: [
                                    for (final tag in entry.pane.tags)
                                      Chip(label: Text(tag)),
                                  ],
                                ),
                              ],
                              const SizedBox(height: 8),
                              Align(
                                alignment: Alignment.centerRight,
                                child: entry.revived
                                    ? Text('Revived as ${entry.revivedAs}')
                                    : FilledButton.tonalIcon(
                                        onPressed: loading
                                            ? null
                                            : () => revive(entry),
                                        icon: const Icon(Icons.restore),
                                        label: const Text('Revive'),
                                      ),
                              ),
                            ],
                          ),
                        ),
                      );
                    },
                  ),
          ),
        ],
      ),
    );
  }
}
