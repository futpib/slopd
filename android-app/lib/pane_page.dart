import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';

import 'control.dart';
import 'dialogs.dart';
import 'models.dart';
import 'ui.dart';

class PanePage extends StatefulWidget {
  const PanePage({super.key, required this.api, required this.pane});

  final ControlConnection api;
  final PaneInfo pane;

  @override
  State<PanePage> createState() => _PanePageState();
}

class _PanePageState extends State<PanePage> with WidgetsBindingObserver {
  final prompt = TextEditingController();
  final scroll = ScrollController();
  late PaneInfo pane = widget.pane;
  List<TranscriptEntry> records = const [];
  String error = '';
  bool loading = false,
      refreshing = false,
      loadingEarlier = false,
      hasEarlier = true,
      sending = false,
      interruptFirst = false,
      active = true,
      openingLive = false;
  int sendTimeout = 60;
  Timer? timer;
  CommandStream? live;
  StreamSubscription<String>? liveLines, liveErrors;
  int liveGeneration = 0;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    widget.api.addListener(_connectionChanged);
    _resume();
  }

  @override
  void dispose() {
    active = false;
    _stopLive();
    timer?.cancel();
    widget.api.removeListener(_connectionChanged);
    WidgetsBinding.instance.removeObserver(this);
    prompt.dispose();
    scroll.dispose();
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    active = state == AppLifecycleState.resumed;
    if (active) {
      _resume();
    } else {
      timer?.cancel();
      timer = null;
      _stopLive();
    }
  }

  void _connectionChanged() {
    if (!mounted) return;
    setState(() {});
    if (widget.api.disconnected || widget.api.reconnecting) {
      _stopLive();
    } else if (active) {
      _resume();
    }
  }

  void _resume() {
    if (!active || widget.api.disconnected || widget.api.reconnecting) return;
    timer ??= Timer.periodic(
      const Duration(seconds: 3),
      (_) => _refreshMetadata(quiet: true),
    );
    refresh(quiet: records.isNotEmpty);
    _startLive();
  }

  Future<void> reconnect() async {
    setState(() => error = '');
    try {
      await widget.api.reconnect();
      _resume();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> refresh({bool quiet = false}) async {
    if (refreshing || widget.api.disconnected || widget.api.reconnecting) {
      return;
    }
    refreshing = true;
    if (!quiet && mounted) setState(() => loading = true);
    try {
      final results = await Future.wait([
        widget.api.command(['transcript', pane.id, '--limit', '100']),
        widget.api.command(['ps', '--json']),
      ]);
      final value = jsonDecode(results[0].stdout) as Map<String, dynamic>;
      final next = (value['records'] as List? ?? const [])
          .map(
            (record) =>
                TranscriptEntry.fromJson(record as Map<String, dynamic>),
          )
          .toList();
      final panes = (jsonDecode(results[1].stdout) as List).map(
        (value) => PaneInfo.fromJson(value as Map<String, dynamic>),
      );
      final updated = panes.where((value) => value.id == pane.id).firstOrNull;
      final shouldScroll = records.isEmpty || _nearBottom;
      if (mounted) {
        setState(() {
          records = _merge(next, records);
          hasEarlier = next.length >= 100;
          if (updated != null) pane = updated;
          error = updated == null ? 'This pane is no longer managed.' : '';
        });
        if (shouldScroll) _scrollToBottom();
      }
    } on ConnectionLost {
      if (mounted) setState(() {});
    } catch (exception) {
      if (mounted) setState(() => error = 'Refresh failed: $exception');
    } finally {
      refreshing = false;
      if (!quiet && mounted) setState(() => loading = false);
    }
  }

  Future<void> _refreshMetadata({bool quiet = false}) async {
    if (refreshing || widget.api.disconnected || widget.api.reconnecting) {
      return;
    }
    try {
      final result = await widget.api.command(['ps', '--json']);
      final panes = (jsonDecode(result.stdout) as List).map(
        (value) => PaneInfo.fromJson(value as Map<String, dynamic>),
      );
      final updated = panes.where((value) => value.id == pane.id).firstOrNull;
      if (mounted) {
        setState(() {
          if (updated != null) pane = updated;
          error = updated == null ? 'This pane is no longer managed.' : '';
        });
      }
    } on ConnectionLost {
      if (mounted) setState(() {});
    } catch (exception) {
      if (mounted) setState(() => error = 'Refresh failed: $exception');
    }
  }

  Future<void> _startLive() async {
    if (live != null ||
        openingLive ||
        !active ||
        widget.api.disconnected ||
        widget.api.reconnecting) {
      return;
    }
    openingLive = true;
    final generation = ++liveGeneration;
    try {
      final stream = await widget.api.openStream([
        'listen',
        '--pane-id',
        pane.id,
        '--replay',
        '0',
      ]);
      if (!mounted || generation != liveGeneration || !active) {
        stream.close();
        return;
      }
      live = stream;
      liveLines = stream.lines.listen(_receiveLive);
      liveErrors = stream.errors.listen((line) {
        if (mounted && line.trim().isNotEmpty) {
          setState(() => error = line.trim());
        }
      });
      unawaited(
        stream.done
            .then<void>((_) {
              if (mounted && generation == liveGeneration) {
                _clearLive();
                setState(
                  () =>
                      error = 'Live transcript ended. Refresh to reconnect it.',
                );
              }
            })
            .catchError((Object exception) {
              if (mounted && generation == liveGeneration) {
                _clearLive();
                setState(() => error = 'Live transcript stopped: $exception');
              }
            }),
      );
    } catch (exception) {
      if (mounted && generation == liveGeneration) {
        setState(() => error = 'Live transcript failed: $exception');
      }
    } finally {
      openingLive = false;
    }
  }

  void _receiveLive(String line) {
    if (line.trim().isEmpty) return;
    try {
      final value = jsonDecode(line) as Map<String, dynamic>;
      if (value['subscribed'] == true || value['event_type'] == 'ReplayEnd') {
        return;
      }
      final record = TranscriptEntry.fromJson(value);
      if (record.source != 'transcript') return;
      final shouldScroll = _nearBottom;
      setState(() {
        records = _merge(records, [record]);
        error = '';
      });
      if (shouldScroll) _scrollToBottom();
    } catch (exception) {
      setState(() => error = 'Invalid live record: $exception');
    }
  }

  List<TranscriptEntry> _merge(
    Iterable<TranscriptEntry> first,
    Iterable<TranscriptEntry> second,
  ) {
    final result = <TranscriptEntry>[], cursors = <int>{};
    for (final record in [...first, ...second]) {
      final cursor = record.cursor;
      if (cursor == null || cursors.add(cursor)) result.add(record);
    }
    result.sort((a, b) => (a.cursor ?? 1 << 62).compareTo(b.cursor ?? 1 << 62));
    return result;
  }

  bool get _nearBottom =>
      !scroll.hasClients ||
      scroll.position.maxScrollExtent - scroll.position.pixels < 160;

  void _scrollToBottom() {
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted && scroll.hasClients) {
        scroll.animateTo(
          scroll.position.maxScrollExtent,
          duration: const Duration(milliseconds: 180),
          curve: Curves.easeOut,
        );
      }
    });
  }

  void _clearLive() {
    liveLines?.cancel();
    liveErrors?.cancel();
    liveLines = null;
    liveErrors = null;
    live = null;
  }

  void _stopLive() {
    liveGeneration++;
    final stream = live;
    _clearLive();
    stream?.close();
  }

  Future<void> loadEarlier() async {
    final cursor = records
        .map((record) => record.cursor)
        .whereType<int>()
        .firstOrNull;
    if (cursor == null || loadingEarlier || !hasEarlier) return;
    setState(() => loadingEarlier = true);
    final oldExtent = scroll.hasClients ? scroll.position.maxScrollExtent : 0.0;
    final oldOffset = scroll.hasClients ? scroll.position.pixels : 0.0;
    try {
      final result = await widget.api.command([
        'transcript',
        pane.id,
        '--before',
        '$cursor',
        '--limit',
        '100',
      ]);
      final value = jsonDecode(result.stdout) as Map<String, dynamic>;
      final earlier = (value['records'] as List? ?? const [])
          .map(
            (record) =>
                TranscriptEntry.fromJson(record as Map<String, dynamic>),
          )
          .toList();
      if (mounted) {
        setState(() {
          records = _merge(earlier, records);
          hasEarlier = earlier.length >= 100;
          error = '';
        });
        WidgetsBinding.instance.addPostFrameCallback((_) {
          if (mounted && scroll.hasClients) {
            final added = scroll.position.maxScrollExtent - oldExtent;
            scroll.jumpTo(
              (oldOffset + added).clamp(0, scroll.position.maxScrollExtent),
            );
          }
        });
      }
    } catch (exception) {
      if (mounted) {
        setState(() => error = 'Could not load earlier records: $exception');
      }
    } finally {
      if (mounted) setState(() => loadingEarlier = false);
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
        '--timeout',
        '$sendTimeout',
        pane.id,
        text,
      ], timeout: Duration(seconds: sendTimeout + 15));
      prompt.clear();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    } finally {
      if (mounted) setState(() => sending = false);
    }
  }

  Future<void> forkPane() async {
    final options = await showLaunchDialog(context, fork: true);
    if (options == null || !mounted) return;
    setState(() => loading = true);
    try {
      final result = await widget.api.command(
        options.forkArguments(pane.id),
        timeout: Duration(
          seconds: options.noWait ? 45 : options.readyTimeout + 20,
        ),
      );
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(content: Text('Forked ${result.stdout.trim()}')),
        );
      }
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    } finally {
      if (mounted) setState(() => loading = false);
    }
  }

  Future<void> manageTags() async {
    final next = await showTagsDialog(context, pane.tags);
    if (next == null || !mounted) return;
    final current = pane.tags.toSet(), desired = next.toSet();
    try {
      for (final tag in current.difference(desired)) {
        await widget.api.command(['untag', pane.id, tag]);
      }
      for (final tag in desired.difference(current)) {
        await widget.api.command(['tag', pane.id, tag]);
      }
      await _refreshMetadata();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> interrupt() async {
    try {
      await widget.api.command(['interrupt', pane.id]);
      await _refreshMetadata();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> kill() async {
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
      if (mounted) Navigator.pop(context);
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Future<void> setSendTimeout() async {
    final controller = TextEditingController(text: '$sendTimeout');
    final value = await showDialog<int>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Send timeout'),
        content: TextField(
          controller: controller,
          keyboardType: TextInputType.number,
          decoration: const InputDecoration(labelText: 'Seconds'),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () {
              final seconds = int.tryParse(controller.text.trim());
              if (seconds != null && seconds > 0) {
                Navigator.pop(context, seconds);
              }
            },
            child: const Text('Save'),
          ),
        ],
      ),
    );
    controller.dispose();
    if (value != null && mounted) setState(() => sendTimeout = value);
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(
      title: Text(pane.id),
      actions: [
        IconButton(
          tooltip: 'Refresh',
          onPressed: loading
              ? null
              : () {
                  refresh();
                  _startLive();
                },
          icon: const Icon(Icons.refresh),
        ),
        PopupMenuButton<String>(
          onSelected: (value) => switch (value) {
            'fork' => forkPane(),
            'tags' => manageTags(),
            'interrupt' => interrupt(),
            'kill' => kill(),
            _ => Future<void>.value(),
          },
          itemBuilder: (_) => const [
            PopupMenuItem(value: 'fork', child: Text('Fork')),
            PopupMenuItem(value: 'tags', child: Text('Manage tags')),
            PopupMenuItem(value: 'interrupt', child: Text('Interrupt')),
            PopupMenuItem(value: 'kill', child: Text('Kill')),
          ],
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
          padding: const EdgeInsets.fromLTRB(16, 8, 16, 0),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              Text(pane.title, style: Theme.of(context).textTheme.titleMedium),
              const SizedBox(height: 4),
              Text(
                '${pane.backend}/${pane.account} · ${pane.detailedState}\n${pane.directory}',
                style: Theme.of(context).textTheme.bodySmall,
              ),
              if (pane.tags.isNotEmpty)
                Padding(
                  padding: const EdgeInsets.only(top: 8),
                  child: Wrap(
                    spacing: 6,
                    runSpacing: 4,
                    children: [
                      for (final tag in pane.tags) Chip(label: Text(tag)),
                    ],
                  ),
                ),
              ExpansionTile(
                tilePadding: EdgeInsets.zero,
                title: const Text('Pane details'),
                childrenPadding: const EdgeInsets.only(bottom: 8),
                expandedCrossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  SelectableText(
                    'Session: ${pane.sessionId.isEmpty ? 'unknown' : pane.sessionId}\n'
                    'Parent: ${pane.parentPaneId.isEmpty ? 'none' : pane.parentPaneId}\n'
                    'Created: ${formatAge(pane.createdAt)}\n'
                    'Last active: ${formatAge(pane.lastActive)}\n'
                    'Transcript: ${pane.transcriptPath.isEmpty ? 'unknown' : pane.transcriptPath}',
                    style: mono.copyWith(fontSize: 11),
                  ),
                ],
              ),
              if (error.isNotEmpty) ErrorBox(error),
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
                  controller: scroll,
                  padding: const EdgeInsets.all(16),
                  itemCount: records.length + (hasEarlier ? 1 : 0),
                  itemBuilder: (context, index) {
                    if (hasEarlier && index == 0) {
                      return Center(
                        child: TextButton.icon(
                          onPressed: loadingEarlier ? null : loadEarlier,
                          icon: loadingEarlier
                              ? const SizedBox.square(
                                  dimension: 16,
                                  child: CircularProgressIndicator(
                                    strokeWidth: 2,
                                  ),
                                )
                              : const Icon(Icons.expand_less),
                          label: const Text('Load earlier'),
                        ),
                      );
                    }
                    final record = records[index - (hasEarlier ? 1 : 0)];
                    return Card(
                      child: Padding(
                        padding: const EdgeInsets.all(14),
                        child: Column(
                          crossAxisAlignment: CrossAxisAlignment.start,
                          children: [
                            Text(
                              '${record.type}${record.cursor == null ? '' : ' · ${record.cursor}'}',
                              style: Theme.of(context).textTheme.labelMedium,
                            ),
                            if (record.timestamp.isNotEmpty)
                              Text(
                                record.timestamp,
                                style: Theme.of(context).textTheme.labelSmall,
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
                      decoration: InputDecoration(
                        labelText: 'Prompt or local command',
                        hintText: 'Continue, or ! command',
                        helperText: 'Timeout: ${sendTimeout}s',
                      ),
                    ),
                  ),
                  const SizedBox(width: 8),
                  Column(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      IconButton(
                        tooltip: 'Set send timeout',
                        onPressed: sending ? null : setSendTimeout,
                        icon: const Icon(Icons.timer_outlined),
                      ),
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
