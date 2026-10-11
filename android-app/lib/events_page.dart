import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';

import 'control.dart';
import 'models.dart';
import 'operations.dart';
import 'ui.dart';

class EventsPage extends StatefulWidget {
  const EventsPage({super.key, required this.api});

  final ControlConnection api;

  @override
  State<EventsPage> createState() => _EventsPageState();
}

class _EventsPageState extends State<EventsPage> with WidgetsBindingObserver {
  final hooks = TextEditingController();
  final events = TextEditingController();
  final transcripts = TextEditingController();
  final pane = TextEditingController();
  final session = TextEditingController();
  final where = TextEditingController();
  final until = TextEditingController();
  final timeout = TextEditingController(text: '60');
  final scroll = ScrollController();
  List<TranscriptEntry> records = const [];
  CommandStream? stream;
  StreamSubscription<String>? lines, errors;
  String error = '';
  bool wait = false,
      noSnapshot = false,
      active = true,
      restartOnResume = false,
      opening = false;
  int generation = 0;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    widget.api.addListener(_connectionChanged);
  }

  @override
  void dispose() {
    active = false;
    stop();
    widget.api.removeListener(_connectionChanged);
    WidgetsBinding.instance.removeObserver(this);
    for (final controller in [
      hooks,
      events,
      transcripts,
      pane,
      session,
      where,
      until,
      timeout,
      scroll,
    ]) {
      controller.dispose();
    }
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    active = state == AppLifecycleState.resumed;
    if (!active) {
      restartOnResume = stream != null;
      stop(clearIntent: false);
    } else if (restartOnResume && !widget.api.disconnected) {
      start();
    }
  }

  void _connectionChanged() {
    if (!mounted) return;
    setState(() {});
    if (widget.api.disconnected || widget.api.reconnecting) {
      restartOnResume = stream != null || restartOnResume;
      stop(clearIntent: false);
    }
  }

  EventOptions? _options() {
    final seconds = int.tryParse(timeout.text.trim());
    if (wait && (seconds == null || seconds < 0)) {
      setState(() => error = 'Wait timeout must be zero or greater.');
      return null;
    }
    if (pane.text.trim().isNotEmpty && session.text.trim().isNotEmpty) {
      setState(() => error = 'Choose a pane ID or a session ID, not both.');
      return null;
    }
    return EventOptions(
      wait: wait,
      hooks: nonEmptyLines(hooks.text),
      events: nonEmptyLines(events.text),
      transcripts: nonEmptyLines(transcripts.text),
      paneId: pane.text.trim(),
      sessionId: session.text.trim(),
      where: nonEmptyLines(where.text),
      until: nonEmptyLines(until.text),
      timeout: seconds ?? 60,
      noSnapshot: noSnapshot,
    );
  }

  Future<void> start() async {
    if (stream != null ||
        opening ||
        widget.api.disconnected ||
        widget.api.reconnecting) {
      return;
    }
    final options = _options();
    if (options == null) return;
    final current = ++generation;
    setState(() {
      error = '';
      restartOnResume = true;
      opening = true;
    });
    try {
      final next = await widget.api.openStream(options.arguments());
      if (!mounted || current != generation || !active) {
        next.close();
        return;
      }
      stream = next;
      lines = next.lines.listen(_receive);
      errors = next.errors.listen((line) {
        if (line.trim().isNotEmpty && mounted) {
          setState(() => error = line.trim());
        }
      });
      setState(() {});
      unawaited(
        next.done
            .then<void>((_) {
              if (mounted && current == generation) {
                _clearStream();
                setState(() => restartOnResume = false);
              }
            })
            .catchError((Object exception) {
              if (mounted && current == generation) {
                _clearStream();
                setState(() {
                  error = 'Event stream stopped: $exception';
                  restartOnResume = false;
                });
              }
            }),
      );
    } catch (exception) {
      if (mounted && current == generation) {
        setState(() {
          error = '$exception';
          restartOnResume = false;
          opening = false;
        });
      }
    } finally {
      if (mounted && current == generation) setState(() => opening = false);
    }
  }

  void _receive(String line) {
    if (line.trim().isEmpty) return;
    try {
      final value = jsonDecode(line) as Map<String, dynamic>;
      if (value['subscribed'] == true) return;
      final record = TranscriptEntry.fromJson(value);
      setState(() {
        records = [...records, record];
        if (records.length > 500) {
          records = records.sublist(records.length - 500);
        }
        error = '';
      });
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted && scroll.hasClients) {
          scroll.animateTo(
            scroll.position.maxScrollExtent,
            duration: const Duration(milliseconds: 160),
            curve: Curves.easeOut,
          );
        }
      });
    } catch (exception) {
      setState(() => error = 'Invalid event record: $exception');
    }
  }

  void _clearStream() {
    lines?.cancel();
    errors?.cancel();
    lines = null;
    errors = null;
    stream = null;
  }

  void stop({bool clearIntent = true}) {
    generation++;
    opening = false;
    final current = stream;
    _clearStream();
    current?.close();
    if (clearIntent) restartOnResume = false;
    if (mounted) setState(() {});
  }

  Future<void> reconnect() async {
    try {
      await widget.api.reconnect();
      if (restartOnResume) await start();
    } catch (exception) {
      if (mounted) setState(() => error = '$exception');
    }
  }

  Widget _linesField(TextEditingController controller, String label) =>
      TextField(
        controller: controller,
        minLines: 1,
        maxLines: 3,
        enabled: stream == null,
        autocorrect: false,
        decoration: InputDecoration(
          labelText: label,
          helperText: 'One per line',
        ),
      );

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(title: const Text('Events and waits')),
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
        ExpansionTile(
          initiallyExpanded: true,
          title: Text(
            wait ? 'Wait for one matching event' : 'Listen continuously',
          ),
          childrenPadding: const EdgeInsets.fromLTRB(16, 0, 16, 16),
          children: [
            SegmentedButton<bool>(
              segments: const [
                ButtonSegment(value: false, label: Text('Listen')),
                ButtonSegment(value: true, label: Text('Wait')),
              ],
              selected: {wait},
              onSelectionChanged: stream == null
                  ? (value) => setState(() => wait = value.single)
                  : null,
            ),
            const SizedBox(height: 16),
            Row(
              children: [
                Expanded(
                  child: TextField(
                    controller: pane,
                    enabled: stream == null,
                    autocorrect: false,
                    decoration: const InputDecoration(labelText: 'Pane ID'),
                  ),
                ),
                const SizedBox(width: 12),
                Expanded(
                  child: TextField(
                    controller: session,
                    enabled: stream == null,
                    autocorrect: false,
                    decoration: const InputDecoration(labelText: 'Session ID'),
                  ),
                ),
              ],
            ),
            const SizedBox(height: 12),
            _linesField(hooks, 'Hook types'),
            const SizedBox(height: 12),
            _linesField(events, 'slopd event types'),
            const SizedBox(height: 12),
            _linesField(transcripts, 'Transcript record types'),
            const SizedBox(height: 12),
            _linesField(where, 'Server predicates (key=value)'),
            if (wait) ...[
              const SizedBox(height: 12),
              _linesField(until, 'Stop predicates (key=value)'),
              const SizedBox(height: 12),
              TextField(
                controller: timeout,
                enabled: stream == null,
                keyboardType: TextInputType.number,
                decoration: const InputDecoration(
                  labelText: 'Wait timeout (seconds, 0 disables)',
                ),
              ),
              SwitchListTile(
                contentPadding: EdgeInsets.zero,
                title: const Text('Ignore current pane state'),
                value: noSnapshot,
                onChanged: stream == null
                    ? (value) => setState(() => noSnapshot = value)
                    : null,
              ),
            ],
            if (error.isNotEmpty) ErrorBox(error),
            const SizedBox(height: 8),
            Row(
              mainAxisAlignment: MainAxisAlignment.end,
              children: [
                if (records.isNotEmpty)
                  TextButton(
                    onPressed: () => setState(() => records = const []),
                    child: const Text('Clear'),
                  ),
                const SizedBox(width: 8),
                FilledButton.icon(
                  onPressed: widget.api.disconnected || opening
                      ? null
                      : stream == null
                      ? start
                      : stop,
                  icon: Icon(stream == null ? Icons.play_arrow : Icons.stop),
                  label: Text(
                    stream == null ? (wait ? 'Wait' : 'Listen') : 'Stop',
                  ),
                ),
              ],
            ),
          ],
        ),
        Expanded(
          child: records.isEmpty
              ? const EmptyState(
                  Icons.sensors,
                  'No events yet',
                  'Choose filters and start listening or waiting.',
                )
              : ListView.builder(
                  controller: scroll,
                  padding: const EdgeInsets.all(16),
                  itemCount: records.length,
                  itemBuilder: (context, index) {
                    final record = records[index];
                    return Card(
                      child: ListTile(
                        title: Text('${record.source} · ${record.type}'),
                        subtitle: SelectableText(record.text, style: mono),
                      ),
                    );
                  },
                ),
        ),
      ],
    ),
  );
}
