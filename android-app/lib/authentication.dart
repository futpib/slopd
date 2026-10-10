import 'package:dartssh2/dartssh2.dart';
import 'package:flutter/material.dart';
import 'control.dart';

class AuthenticationDialog {
  AuthenticationDialog(this.context, this.target);
  final BuildContext context;
  final String target;
  DialogRoute<dynamic>? _route;
  bool _closed = false;
  final cancellation = AuthenticationCancellation();
  bool get active => !_closed && !cancellation.isCancelled && context.mounted;

  void _dismiss() {
    final route = _route;
    _route = null;
    if (route != null && route.isActive) route.navigator?.removeRoute(route);
  }

  Future<T?> show<T>(WidgetBuilder builder) async {
    if (!active) return null;
    _dismiss();
    final route = DialogRoute<T>(
      context: context,
      barrierDismissible: false,
      builder: builder,
    );
    _route = route;
    try {
      final result = await Navigator.of(context).push(route);
      return active ? result : null;
    } finally {
      if (_route == route) _route = null;
    }
  }

  Future<List<String>?> request(SSHUserInfoRequest request, bool retry) async {
    if (!active) return null;
    _dismiss();
    final route = DialogRoute<List<String>>(
      context: context,
      barrierDismissible: false,
      builder: (_) =>
          _AuthenticationForm(target: target, request: request, retry: retry),
    );
    _route = route;
    final response = Navigator.of(context).push(route);
    if (request.prompts.isEmpty) {
      response.then((_) {
        if (_route == route && active) cancellation.cancel();
        if (_route == route) _route = null;
      });
      return [];
    }
    try {
      final answers = await response;
      if (answers == null && active) cancellation.cancel();
      return active ? answers : null;
    } finally {
      if (_route == route) _route = null;
    }
  }

  void close() {
    _closed = true;
    cancellation.cancel();
    _dismiss();
  }
}

class _AuthenticationForm extends StatefulWidget {
  const _AuthenticationForm({
    required this.target,
    required this.request,
    required this.retry,
  });
  final String target;
  final SSHUserInfoRequest request;
  final bool retry;

  @override
  State<_AuthenticationForm> createState() => _AuthenticationFormState();
}

class _AuthenticationFormState extends State<_AuthenticationForm> {
  String? error;
  late final fields = [
    for (final _ in widget.request.prompts) TextEditingController(),
  ];

  @override
  void dispose() {
    for (final field in fields) {
      field.dispose();
    }
    super.dispose();
  }

  void submit() {
    if (widget.request is PasswordChangeRequest &&
        (fields[1].text.isEmpty || fields[1].text != fields[2].text)) {
      setState(
        () => error = fields[1].text.isEmpty
            ? 'Enter a new password.'
            : 'The new passwords do not match.',
      );
      return;
    }
    Navigator.pop(context, [for (final f in fields) f.text]);
  }

  @override
  Widget build(BuildContext context) => AlertDialog(
    title: Text(
      widget.request.name.isEmpty ? 'SSH authentication' : widget.request.name,
    ),
    content: SingleChildScrollView(
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text(widget.target),
          if (widget.retry) ...[
            const SizedBox(height: 12),
            const Text('Authentication failed. Try again.'),
          ],
          if (widget.request.instruction.isNotEmpty) ...[
            const SizedBox(height: 12),
            Text(widget.request.instruction),
          ],
          for (var i = 0; i < fields.length; i++) ...[
            const SizedBox(height: 16),
            TextField(
              controller: fields[i],
              autofocus: i == 0,
              obscureText: !widget.request.prompts[i].echo,
              autocorrect: false,
              enableSuggestions: false,
              decoration: InputDecoration(
                labelText: widget.request.prompts[i].promptText,
              ),
              textInputAction: i == fields.length - 1
                  ? TextInputAction.done
                  : TextInputAction.next,
              onSubmitted: i == fields.length - 1 ? (_) => submit() : null,
            ),
          ],
          if (fields.isEmpty) ...[
            const SizedBox(height: 16),
            const Text('Waiting for the server…'),
          ],
          if (error != null)
            Text(
              error!,
              style: TextStyle(color: Theme.of(context).colorScheme.error),
            ),
        ],
      ),
    ),
    actions: [
      TextButton(
        onPressed: () => Navigator.pop(context),
        child: const Text('Cancel'),
      ),
      if (fields.isNotEmpty)
        FilledButton(onPressed: submit, child: const Text('Continue')),
    ],
  );
}
