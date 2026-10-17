import 'dart:convert';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'control.dart';
import 'authentication.dart';
import 'dashboard.dart';
import 'identities.dart';
import 'keys_page.dart';
import 'qr_scan.dart';
import 'ui.dart';

class Target {
  const Target(this.user, this.host, this.port);
  final String user, host;
  final int port;
  static Target parse(String input, bool iroh) {
    final text = input.trim();
    if (iroh) {
      final match = RegExp(r'^([^\s@]+)@([a-fA-F0-9]{64})$').firstMatch(text);
      if (match == null) {
        throw const FormatException(
          'Use user@ followed by a 64-character Iroh endpoint ID.',
        );
      }
      return Target(match[1]!, match[2]!.toLowerCase(), 22);
    }
    try {
      final address = RegExp(
        r'^(?:ssh://)?([^\s@:/]+)@(\[[^\]\s]+\]|[^\s@:/?#]+)(?::([0-9]+))?$',
      ).firstMatch(text);
      if (address == null) throw const FormatException();
      final port = address[3] == null ? 22 : int.parse(address[3]!);
      if (port < 1 || port > 65535) throw const FormatException();
      final uri = Uri.parse(text.startsWith('ssh://') ? text : 'ssh://$text');
      if (uri.scheme != 'ssh' ||
          uri.userInfo.isEmpty ||
          uri.userInfo.contains(':') ||
          uri.host.isEmpty ||
          uri.path.isNotEmpty ||
          uri.hasQuery ||
          uri.hasFragment ||
          (uri.hasPort && (uri.port < 1 || uri.port > 65535)) ||
          RegExp(r'\s').hasMatch(text)) {
        throw const FormatException();
      }
      return Target(
        Uri.decodeComponent(uri.userInfo),
        uri.host.replaceAll(RegExp(r'[\[\]]'), ''),
        port,
      );
    } catch (_) {
      throw const FormatException(
        'Use user@host, user@host:port, or user@[IPv6]:port.',
      );
    }
  }

  static String of(Profile p) =>
      '${p.user}@${p.host.contains(':') ? '[${p.host}]' : p.host}${!p.iroh && p.port != 22 ? ':${p.port}' : ''}';
}

class TransportSelector extends StatelessWidget {
  const TransportSelector({
    super.key,
    required this.iroh,
    required this.onChanged,
  });
  final bool iroh;
  final ValueChanged<bool>? onChanged;
  @override
  Widget build(BuildContext context) => SegmentedButton<bool>(
    segments: const [
      ButtonSegment(
        value: true,
        icon: Icon(Icons.cloud_outlined),
        label: Text('Iroh'),
      ),
      ButtonSegment(
        value: false,
        icon: Icon(Icons.computer_outlined),
        label: Text('SSH'),
      ),
    ],
    selected: {iroh},
    onSelectionChanged: onChanged == null ? null : (v) => onChanged!(v.single),
  );
}

class ConnectionsPage extends StatefulWidget {
  const ConnectionsPage({super.key});
  @override
  State<ConnectionsPage> createState() => _ConnectionsPageState();
}

class _ConnectionsPageState extends State<ConnectionsPage> {
  final store = const ProfileStore(), target = TextEditingController();
  List<Profile> profiles = [];
  List<Identity> keys = [];
  String? keyId, error;
  bool busy = false, iroh = true;
  AuthenticationDialog? authentication;
  @override
  void initState() {
    super.initState();
    load();
  }

  @override
  void dispose() {
    authentication?.close();
    target.dispose();
    super.dispose();
  }

  Future<void> load() async {
    try {
      final saved = await store.load();
      final identities = await IdentityStore().load();
      if (mounted) {
        setState(() {
          profiles = saved;
          keys = identities;
          if (keyId == null) {
            keyId = keys.length == 1 ? keys.single.id : '';
          } else if (keyId!.isNotEmpty && !keys.any((k) => k.id == keyId)) {
            keyId = '';
          }
        });
      }
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    }
  }

  Future<void> manageKeys() async {
    await Navigator.of(
      context,
    ).push<void>(MaterialPageRoute(builder: (_) => const KeysPage()));
    await load();
  }

  Future<void> edit([Profile? original]) async {
    final result = await Navigator.of(context).push<Profile>(
      MaterialPageRoute(builder: (_) => ProfilePage(profile: original)),
    );
    if (result == null) return;
    try {
      final updated = [...profiles];
      if (original == null || !updated.contains(original)) {
        updated.add(result);
      } else {
        updated[updated.indexOf(original)] = result;
      }
      await store.save(updated);
      await load();
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    }
  }

  Future<void> quickConnect() async {
    try {
      final address = Target.parse(target.text, iroh);
      final p = Profile(
        name: address.host,
        host: address.host,
        user: address.user,
        port: address.port,
        iroh: iroh,
        keyId: keyId ?? '',
      );
      await connect(p, save: true);
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    }
  }

  Future<void> connect(Profile profile, {bool save = false}) async {
    setState(() {
      busy = true;
      error = null;
    });
    Future<ControlApi> open() async {
      final dialog = AuthenticationDialog(context, Target.of(profile));
      authentication = dialog;
      return SshControl.connect(
        profile,
        (type, fingerprint) async {
          final saved = await store.hostKey(profile.hostIdentity),
              key = '$type $fingerprint';
          if (saved != null) {
            if (saved != key) {
              throw StateError(
                'SSH host key changed for ${profile.host}.\nSaved: $saved\nReceived: $key',
              );
            }
            return true;
          }
          if (!mounted) return false;
          final trusted =
              await dialog.show<bool>(
                (context) => AlertDialog(
                  icon: const Icon(Icons.fingerprint, size: 32),
                  title: const Text('Trust SSH host key?'),
                  content: SingleChildScrollView(
                    child: Column(
                      mainAxisSize: MainAxisSize.min,
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(
                          profile.name,
                          style: Theme.of(context).textTheme.titleMedium,
                        ),
                        const SizedBox(height: 8),
                        Text(
                          profile.host,
                          maxLines: 3,
                          overflow: TextOverflow.ellipsis,
                        ),
                        const SizedBox(height: 20),
                        Text(
                          type,
                          style: Theme.of(context).textTheme.labelMedium,
                        ),
                        const SizedBox(height: 8),
                        SelectableText(fingerprint, style: mono),
                        const SizedBox(height: 20),
                        const Text(
                          'Compare this fingerprint with your server. Future connections must use the same key.',
                        ),
                      ],
                    ),
                  ),
                  actions: [
                    TextButton(
                      onPressed: () => Navigator.pop(context, false),
                      child: const Text('Cancel'),
                    ),
                    FilledButton(
                      onPressed: () => Navigator.pop(context, true),
                      child: const Text('Trust'),
                    ),
                  ],
                ),
              ) ??
              false;
          if (trusted && dialog.active) {
            await store.trust(profile.hostIdentity, key);
          }
          return trusted && dialog.active;
        },
        onAuthenticationRequest: dialog.request,
        cancellation: dialog.cancellation,
      ).whenComplete(() {
        dialog.close();
        if (authentication == dialog) authentication = null;
      });
    }

    try {
      final api = ControlConnection(await open(), open);
      try {
        if (save &&
            !profiles.any(
              (p) =>
                  p.hostIdentity == profile.hostIdentity &&
                  p.user == profile.user,
            )) {
          await store.save([...profiles, profile]);
          await load();
        }
        if (!mounted) return;
        await Navigator.of(context).push<void>(
          MaterialPageRoute(
            builder: (_) => Dashboard(
              api: api,
              name: profile.name,
              connection: '${profile.user} · ${profile.iroh ? 'Iroh' : 'SSH'}',
            ),
          ),
        );
      } finally {
        await api.close();
        api.dispose();
      }
    } on AuthenticationCancelled {
      // Cancelling authentication returns to the connection list.
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    }
    if (mounted) setState(() => busy = false);
  }

  Future<void> forget(Profile profile, bool keyOnly) async {
    final yes = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text(
          keyOnly ? 'Forget trusted host key?' : 'Delete saved connection?',
        ),
        content: Text(
          keyOnly
              ? 'Verify the server fingerprint again on your next connection.'
              : 'Remove ${profile.name}? Your SSH identity stays in SSH keys.',
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
          TextButton(
            onPressed: () => Navigator.pop(context, true),
            child: Text(keyOnly ? 'Forget' : 'Delete'),
          ),
        ],
      ),
    );
    if (yes != true) return;
    try {
      if (keyOnly) {
        await store.forget(profile.hostIdentity);
      } else {
        await store.save([...profiles]..remove(profile));
        await load();
      }
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    }
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(
      title: const Text('slopd'),
      actions: [
        IconButton(
          tooltip: 'SSH keys',
          onPressed: busy ? null : manageKeys,
          icon: const Icon(Icons.key_outlined),
        ),
      ],
    ),
    body: ListView(
      padding: const EdgeInsets.all(20),
      children: [
        const SizedBox(height: 12),
        TransportSelector(
          iroh: iroh,
          onChanged: busy ? null : (v) => setState(() => iroh = v),
        ),
        const SizedBox(height: 24),
        TextField(
          controller: target,
          enabled: !busy,
          autocorrect: false,
          enableSuggestions: false,
          decoration: InputDecoration(
            labelText: 'Target',
            hintText: iroh ? 'user@endpoint_id' : 'user@host:port',
            suffixIcon: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                QrScanButton(
                  controller: target,
                  label: 'target',
                  enabled: !busy,
                  trim: true,
                ),
                IconButton(
                  tooltip: 'Paste target',
                  icon: const Icon(Icons.content_paste),
                  onPressed: busy
                      ? null
                      : () async {
                          final data = await Clipboard.getData(
                            Clipboard.kTextPlain,
                          );
                          if (mounted && data != null) {
                            target.text = data.text?.trim() ?? '';
                          }
                        },
                ),
              ],
            ),
          ),
          onSubmitted: (_) => busy ? null : quickConnect(),
        ),
        const SizedBox(height: 16),
        if (keys.isNotEmpty)
          DropdownButtonFormField<String>(
            key: ValueKey(keyId),
            initialValue: keyId,
            isExpanded: true,
            decoration: const InputDecoration(
              labelText: 'Authentication',
              prefixIcon: Icon(Icons.key_outlined),
            ),
            items: [
              const DropdownMenuItem(
                value: '',
                child: Text('Password / no SSH key'),
              ),
              for (final k in keys)
                DropdownMenuItem(
                  value: k.id,
                  child: Text(k.label, overflow: TextOverflow.ellipsis),
                ),
            ],
            onChanged: busy ? null : (v) => setState(() => keyId = v),
          ),
        if (keys.isEmpty)
          ListTile(
            contentPadding: EdgeInsets.zero,
            leading: const Icon(Icons.key_outlined),
            title: const Text('Add an SSH key'),
            subtitle: const Text('Import or generate an identity'),
            trailing: const Icon(Icons.chevron_right),
            onTap: busy ? null : manageKeys,
          ),
        Align(
          alignment: Alignment.centerRight,
          child: TextButton.icon(
            onPressed: busy
                ? null
                : () async {
                    Profile? p;
                    if (target.text.trim().isNotEmpty) {
                      try {
                        final t = Target.parse(target.text, iroh);
                        p = Profile(
                          name: '',
                          host: t.host,
                          user: t.user,
                          port: t.port,
                          iroh: iroh,
                          keyId: keyId ?? '',
                        );
                      } catch (e) {
                        setState(() => error = '$e');
                        return;
                      }
                    }
                    await edit(p);
                  },
            icon: const Icon(Icons.tune, size: 18),
            label: const Text('Connection options'),
          ),
        ),
        if (error != null) ErrorBox(error!),
        FilledButton.icon(
          onPressed: busy ? null : quickConnect,
          icon: busy
              ? const SizedBox.square(
                  dimension: 18,
                  child: CircularProgressIndicator(strokeWidth: 2),
                )
              : const Icon(Icons.arrow_forward),
          label: Text(busy ? 'Connecting…' : 'Connect'),
        ),
        SectionHeading(
          'Saved connections',
          trailing: IconButton(
            tooltip: 'Add connection',
            onPressed: busy ? null : () => edit(),
            icon: const Icon(Icons.add),
          ),
        ),
        if (profiles.isEmpty)
          const EmptyState(
            Icons.dns_outlined,
            'Your servers, one tap away',
            'Connect above or add a connection with custom settings.',
          ),
        for (final p in profiles)
          Card(
            child: ListTile(
              contentPadding: const EdgeInsets.symmetric(
                horizontal: 16,
                vertical: 8,
              ),
              leading: Icon(
                p.iroh ? Icons.cloud_outlined : Icons.computer_outlined,
              ),
              title: Text(p.name, maxLines: 1, overflow: TextOverflow.ellipsis),
              subtitle: Text(
                Target.of(p),
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
                style: mono.copyWith(fontSize: 12),
              ),
              onTap: busy ? null : () => connect(p),
              trailing: PopupMenuButton<String>(
                enabled: !busy,
                onSelected: (v) =>
                    v == 'edit' ? edit(p) : forget(p, v == 'key'),
                itemBuilder: (_) => const [
                  PopupMenuItem(value: 'edit', child: Text('Edit')),
                  PopupMenuItem(value: 'key', child: Text('Forget host key')),
                  PopupMenuItem(value: 'delete', child: Text('Delete')),
                ],
              ),
            ),
          ),
      ],
    ),
  );
}

class ProfilePage extends StatefulWidget {
  const ProfilePage({super.key, this.profile});
  final Profile? profile;
  @override
  State<ProfilePage> createState() => _ProfilePageState();
}

class _ProfilePageState extends State<ProfilePage> {
  final form = GlobalKey<FormState>();
  late final Map<String, TextEditingController> fields;
  late bool iroh, password;
  List<Identity> keys = [];
  String? keyId, error;
  List<String> additionalKeyIds = [];
  late String initial;
  bool leaving = false, confirming = false;
  String get snapshot => jsonEncode([
    iroh,
    password,
    password ? '' : keyId,
    if (!password) ...additionalKeyIds,
    for (final f in fields.values) f.text,
  ]);
  bool get dirty => snapshot != initial;
  void changed() => setState(() {});

  Future<void> leave() async {
    if (confirming) return;
    confirming = true;
    final discard = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Discard connection changes?'),
        content: const Text('Your changes have not been saved.'),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Keep editing'),
          ),
          FilledButton(
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Discard'),
          ),
        ],
      ),
    );
    confirming = false;
    if (discard == true && mounted) {
      setState(() => leaving = true);
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (mounted) Navigator.pop(context);
      });
    }
  }

  @override
  void initState() {
    super.initState();
    final p = widget.profile;
    iroh = p?.iroh ?? true;
    password =
        p == null ||
        p.password.isNotEmpty ||
        (p.keyId.isEmpty && p.privateKey.isEmpty);
    keyId = p?.keyId.isNotEmpty == true ? p!.keyId : null;
    additionalKeyIds = List.of(p?.additionalKeyIds ?? const []);
    fields = {
      for (final e in {
        'Name': p?.name ?? '',
        'Target': p == null ? '' : Target.of(p),
        'Password': p?.password ?? '',
        'Relays': p?.relays.join('\n') ?? '',
        'Executable': p?.executable ?? 'slopctl',
        'Socket': p?.socket ?? '',
        'Config': p?.config ?? '',
      }.entries)
        e.key: TextEditingController(text: e.value),
    };
    initial = snapshot;
    for (final f in fields.values) {
      f.addListener(changed);
    }
    loadKeys();
  }

  Future<void> loadKeys() async {
    try {
      final result = await IdentityStore().load();
      if (mounted) {
        setState(() {
          keys = result;
          if (keyId == null) {
            final pristine = !dirty;
            keyId = keys.length == 1 ? keys.single.id : null;
            if (pristine) initial = snapshot;
          }
        });
      }
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    }
  }

  @override
  void dispose() {
    for (final f in fields.values) {
      f.dispose();
    }
    super.dispose();
  }

  String value(String key) => fields[key]!.text;
  Widget field(
    String key, {
    String? label,
    String? hint,
    int lines = 1,
    bool secret = false,
    bool required = false,
  }) => Padding(
    padding: const EdgeInsets.only(bottom: 16),
    child: TextFormField(
      controller: fields[key],
      maxLines: lines,
      obscureText: secret,
      autocorrect: false,
      enableSuggestions: false,
      decoration: InputDecoration(
        labelText: label ?? key,
        hintText: hint,
        suffixIcon: ['Target', 'Password', 'Relays'].contains(key)
            ? QrScanButton(
                controller: fields[key]!,
                label: key.toLowerCase(),
                trim: !secret,
              )
            : null,
      ),
      validator: (text) {
        if (required && text!.trim().isEmpty) return 'Required';
        if (key == 'Target') {
          try {
            Target.parse(text!, iroh);
          } catch (e) {
            return (e as FormatException).message;
          }
        }
        return null;
      },
    ),
  );
  void save() {
    if (!form.currentState!.validate()) return;
    if (!password && !keys.any((k) => k.id == keyId)) {
      setState(
        () => error = 'Add an SSH key or choose password authentication.',
      );
      return;
    }
    if (!password &&
        additionalKeyIds.any(
          (id) => id == keyId || !keys.any((key) => key.id == id),
        )) {
      setState(() => error = 'Choose distinct available SSH keys.');
      return;
    }
    final t = Target.parse(value('Target'), iroh);
    final result = Profile(
      name: value('Name').trim().isEmpty ? t.host : value('Name').trim(),
      host: t.host,
      user: t.user,
      port: t.port,
      iroh: iroh,
      keyId: password ? '' : keyId!,
      additionalKeyIds: password ? const [] : additionalKeyIds,
      password: password ? value('Password') : '',
      executable: value('Executable').trim(),
      socket: value('Socket').trim(),
      config: value('Config').trim(),
      relays: value(
        'Relays',
      ).split(RegExp(r'\s+')).where((s) => s.isNotEmpty).toList(),
    );
    setState(() => leaving = true);
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) Navigator.pop(context, result);
    });
  }

  @override
  Widget build(BuildContext context) => PopScope<Profile>(
    canPop: leaving || !dirty,
    onPopInvokedWithResult: (didPop, result) {
      if (!didPop) leave();
    },
    child: Scaffold(
      appBar: AppBar(
        title: Text(
          widget.profile == null ? 'New connection' : 'Connection options',
        ),
      ),
      bottomNavigationBar: SafeArea(
        minimum: const EdgeInsets.all(16),
        child: FilledButton(
          onPressed: save,
          child: const Text('Save connection'),
        ),
      ),
      body: Form(
        key: form,
        child: SingleChildScrollView(
          padding: const EdgeInsets.all(20),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              TransportSelector(
                iroh: iroh,
                onChanged: (v) => setState(() => iroh = v),
              ),
              const SizedBox(height: 24),
              field(
                'Target',
                hint: iroh ? 'user@endpoint_id' : 'user@host:port',
                required: true,
              ),
              field(
                'Name',
                label: 'Display name (optional)',
                hint: 'Home server',
              ),
              const SectionHeading('Authentication'),
              SegmentedButton<bool>(
                segments: const [
                  ButtonSegment(
                    value: false,
                    icon: Icon(Icons.key),
                    label: Text('SSH key'),
                  ),
                  ButtonSegment(
                    value: true,
                    icon: Icon(Icons.password),
                    label: Text('Password'),
                  ),
                ],
                selected: {password},
                onSelectionChanged: (v) => setState(() => password = v.single),
              ),
              const SizedBox(height: 16),
              if (password)
                field(
                  'Password',
                  label: 'Password (optional)',
                  hint: 'Ask when connecting',
                  secret: true,
                )
              else ...[
                if (keys.isNotEmpty)
                  DropdownButtonFormField<String>(
                    key: ValueKey(keyId),
                    initialValue: keys.any((k) => k.id == keyId) ? keyId : null,
                    isExpanded: true,
                    decoration: const InputDecoration(labelText: 'Identity'),
                    items: keys
                        .map(
                          (k) => DropdownMenuItem(
                            value: k.id,
                            child: Text(k.label),
                          ),
                        )
                        .toList(),
                    onChanged: (v) => setState(() {
                      keyId = v;
                      additionalKeyIds.remove(v);
                    }),
                  ),
                if (additionalKeyIds.isNotEmpty)
                  Row(
                    children: [
                      const Expanded(child: Text('First key')),
                      IconButton(
                        tooltip: 'Move first key down',
                        onPressed: () => setState(() {
                          final first = keyId!;
                          keyId = additionalKeyIds.first;
                          additionalKeyIds[0] = first;
                        }),
                        icon: const Icon(Icons.arrow_downward),
                      ),
                    ],
                  ),
                for (var index = 0; index < additionalKeyIds.length; index++)
                  Row(
                    key: ValueKey(additionalKeyIds[index]),
                    children: [
                      Expanded(
                        child: Text(
                          '${index + 2}. ${keys.where((k) => k.id == additionalKeyIds[index]).firstOrNull?.label ?? 'Missing key'}',
                          overflow: TextOverflow.ellipsis,
                        ),
                      ),
                      IconButton(
                        tooltip: 'Move key up',
                        onPressed: () => setState(() {
                          final selected = [keyId!, ...additionalKeyIds];
                          final item = selected.removeAt(index + 1);
                          selected.insert(index, item);
                          keyId = selected.first;
                          additionalKeyIds = selected.skip(1).toList();
                        }),
                        icon: const Icon(Icons.arrow_upward),
                      ),
                      if (index < additionalKeyIds.length - 1)
                        IconButton(
                          tooltip: 'Move key down',
                          onPressed: () => setState(() {
                            final item = additionalKeyIds.removeAt(index);
                            additionalKeyIds.insert(index + 1, item);
                          }),
                          icon: const Icon(Icons.arrow_downward),
                        ),
                      IconButton(
                        tooltip: 'Remove key from connection',
                        onPressed: () =>
                            setState(() => additionalKeyIds.removeAt(index)),
                        icon: const Icon(Icons.remove_circle_outline),
                      ),
                    ],
                  ),
                if (keys.any(
                  (k) => k.id != keyId && !additionalKeyIds.contains(k.id),
                ))
                  DropdownButtonFormField<String>(
                    key: ValueKey('add-${additionalKeyIds.join(',')}-$keyId'),
                    decoration: const InputDecoration(
                      labelText: 'Add another key (optional)',
                    ),
                    items: [
                      for (final key in keys.where(
                        (k) =>
                            k.id != keyId && !additionalKeyIds.contains(k.id),
                      ))
                        DropdownMenuItem(value: key.id, child: Text(key.label)),
                    ],
                    onChanged: (id) {
                      if (id != null) setState(() => additionalKeyIds.add(id));
                    },
                  ),
                Align(
                  alignment: Alignment.centerLeft,
                  child: TextButton.icon(
                    onPressed: () async {
                      await Navigator.of(context).push<void>(
                        MaterialPageRoute(builder: (_) => const KeysPage()),
                      );
                      await loadKeys();
                    },
                    icon: const Icon(Icons.key_outlined),
                    label: Text(
                      keys.isEmpty ? 'Add SSH key' : 'Manage SSH keys',
                    ),
                  ),
                ),
              ],
              const SizedBox(height: 8),
              ExpansionTile(
                tilePadding: EdgeInsets.zero,
                title: const Text('Advanced'),
                subtitle: const Text('Remote paths and network'),
                children: [
                  const SizedBox(height: 12),
                  field(
                    'Executable',
                    label: 'Remote slopctl executable',
                    required: true,
                  ),
                  field('Socket', label: 'Control socket (optional)'),
                  field('Config', label: 'Config file (optional)'),
                  if (iroh)
                    field(
                      'Relays',
                      label: 'Relay URLs (optional)',
                      lines: 3,
                      hint: 'One per line; empty uses Iroh defaults',
                    ),
                ],
              ),
              if (error != null) ErrorBox(error!),
            ],
          ),
        ),
      ),
    ),
  );
}
