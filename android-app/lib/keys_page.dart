import 'dart:async';
import 'dart:convert';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'control.dart';
import 'identities.dart';
import 'open_keychain.dart';
import 'qr_scan.dart';
import 'ui.dart';

class KeysPage extends StatefulWidget {
  const KeysPage({super.key});
  @override
  State<KeysPage> createState() => _KeysPageState();
}

class _KeysPageState extends State<KeysPage> {
  final store = IdentityStore();
  List<Identity> keys = [];
  String? error;
  bool importingExternal = false;
  OpenKeychainOperation? importOperation;
  @override
  void dispose() {
    unawaited(importOperation?.cancel());
    super.dispose();
  }

  @override
  void initState() {
    super.initState();
    load();
  }

  Future<void> load() async {
    try {
      final result = await store.load();
      if (mounted) setState(() => keys = result);
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    }
  }

  Future<void> add(bool generate) async {
    final key = await Navigator.of(context).push<Identity>(
      MaterialPageRoute(builder: (_) => AddKeyPage(generate: generate)),
    );
    await load();
    if (key != null && mounted) await publicKey(key);
  }

  Future<void> addExternal() async {
    if (importingExternal) return;
    final operation = OpenKeychainOperation();
    importOperation = operation;
    DialogRoute<OpenKeychainProvider>? providerRoute;
    setState(() {
      importingExternal = true;
      error = null;
    });
    try {
      final client = OpenKeychainClient.instance;
      final providers = await client.listProviders(operation: operation);
      if (!mounted) return;
      if (providers.isEmpty) {
        throw StateError(
          'No OpenKeychain provider is installed. Install OpenKeychain and add a key there first.',
        );
      }
      final provider = providers.length == 1
          ? providers.single
          : await operation.run(
              () => Navigator.of(context).push(
                providerRoute = DialogRoute<OpenKeychainProvider>(
                  context: context,
                  builder: (context) => SimpleDialog(
                    title: const Text('Choose key provider'),
                    children: [
                      for (final provider in providers)
                        SimpleDialogOption(
                          onPressed: () => Navigator.pop(context, provider),
                          child: Text(provider.label),
                        ),
                    ],
                  ),
                ),
              ),
            );
      if (provider == null) return;
      final selection = await client.selectKey(
        provider.packageName,
        operation: operation,
      );
      final publicKeyString = await client.getSshPublicKey(
        operation: operation,
        providerPackage: provider.packageName,
        keyId: selection.keyId,
      );
      // The last provider response may race cancellation/disposal.
      await operation.run(() async {});
      if (!mounted) return;
      // Persistence has begun; this is no longer a provider wait.
      operation.dispose();
      setState(() => importOperation = null);
      final key = await store.importExternal(
        OpenKeychainKey(
          providerPackage: provider.packageName,
          providerLabel: provider.label,
          keyId: selection.keyId,
          description: selection.description,
          publicKeyString: publicKeyString,
        ),
      );
      await load();
      if (mounted) await publicKey(key);
    } on PlatformException catch (e) {
      if (mounted && e.code != 'openkeychain_cancelled') {
        setState(() => error = e.message ?? e.code);
      }
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    } finally {
      operation.dispose();
      if (providerRoute?.isActive == true) {
        providerRoute!.navigator?.removeRoute(providerRoute!);
      }
      if (mounted) {
        setState(() {
          importingExternal = false;
          importOperation = null;
        });
      }
    }
  }

  Future<void> publicKey(Identity key) => showDialog<void>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text(key.name),
      content: SingleChildScrollView(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisSize: MainAxisSize.min,
          children: [
            const Text('Add this public key to your server’s authorized_keys.'),
            const SizedBox(height: 16),
            SelectableText(key.publicKey, style: mono),
            const SizedBox(height: 16),
            Text(key.fingerprint, style: Theme.of(context).textTheme.bodySmall),
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(context),
          child: const Text('Close'),
        ),
        FilledButton.icon(
          icon: const Icon(Icons.copy, size: 18),
          label: const Text('Copy public key'),
          onPressed: () async {
            await Clipboard.setData(ClipboardData(text: key.publicKey));
            if (context.mounted) Navigator.pop(context);
          },
        ),
      ],
    ),
  );
  Future<void> remove(Identity key) async {
    try {
      final profiles = await const ProfileStore().load();
      final users = profiles
          .where((p) => p.selectedKeyIds.contains(key.id))
          .map((p) => p.name)
          .toList();
      if (users.isNotEmpty) {
        throw StateError(
          'This key is used by ${users.join(', ')}. Change those connections before deleting it.',
        );
      }
      if (!mounted) return;
      final yes = await showDialog<bool>(
        context: context,
        builder: (context) => AlertDialog(
          title: const Text('Delete SSH key?'),
          content: Text(
            key.externalKey == null
                ? 'Remove ${key.name} from this device? You cannot recover a generated private key after deleting it.'
                : 'Remove ${key.name} from slopd? The private key stays in ${key.externalKey!.providerLabel}.',
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(context, false),
              child: const Text('Cancel'),
            ),
            TextButton(
              onPressed: () => Navigator.pop(context, true),
              child: const Text('Delete'),
            ),
          ],
        ),
      );
      if (yes != true) return;
      await store.save(
        (await store.load()).where((k) => k.id != key.id).toList(),
      );
      await load();
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    }
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(title: const Text('SSH keys')),
    body: ListView(
      padding: const EdgeInsets.all(20),
      children: [
        Text(
          'One key, all your servers',
          style: Theme.of(context).textTheme.headlineSmall,
        ),
        const SizedBox(height: 8),
        const Text(
          'Import or generate a local key, or use a key held by OpenKeychain. Local private keys stay encrypted on this device; external keys stay with their provider.',
        ),
        const SizedBox(height: 24),
        Row(
          children: [
            Expanded(
              child: FilledButton.icon(
                onPressed: () => add(true),
                icon: const Icon(Icons.add),
                label: const Text('Generate'),
              ),
            ),
            const SizedBox(width: 12),
            Expanded(
              child: OutlinedButton.icon(
                onPressed: () => add(false),
                icon: const Icon(Icons.file_download_outlined),
                label: const Text('Import'),
              ),
            ),
          ],
        ),
        const SizedBox(height: 12),
        OutlinedButton.icon(
          onPressed: importingExternal ? null : addExternal,
          icon: const Icon(Icons.security),
          label: Text(
            importingExternal ? 'Opening provider…' : 'Add from OpenKeychain',
          ),
        ),
        if (importOperation != null)
          TextButton(
            onPressed: () => importOperation?.cancel(),
            child: const Text('Cancel key import'),
          ),
        if (error != null) ...[const SizedBox(height: 16), ErrorBox(error!)],
        SectionHeading('Identities · ${keys.length}'),
        if (keys.isEmpty)
          const EmptyState(
            Icons.key_outlined,
            'No SSH keys yet',
            'Your public key goes on the server. Choose where to keep your private key.',
          ),
        for (final key in keys)
          Card(
            child: ListTile(
              contentPadding: const EdgeInsets.symmetric(
                horizontal: 16,
                vertical: 8,
              ),
              leading: Icon(
                key.externalKey == null ? Icons.key : Icons.security,
              ),
              title: Text(key.name),
              subtitle: Text(
                '${key.externalKey == null ? key.publicKey.split(' ').first : key.externalKey!.providerLabel}\n${key.fingerprint}',
                maxLines: 3,
                overflow: TextOverflow.ellipsis,
                style: mono.copyWith(fontSize: 11),
              ),
              onTap: () => publicKey(key),
              trailing: PopupMenuButton<String>(
                onSelected: (_) => remove(key),
                itemBuilder: (_) => const [
                  PopupMenuItem(value: 'delete', child: Text('Delete')),
                ],
              ),
            ),
          ),
      ],
    ),
  );
}

class AddKeyPage extends StatefulWidget {
  const AddKeyPage({super.key, required this.generate});
  final bool generate;
  @override
  State<AddKeyPage> createState() => _AddKeyPageState();
}

class _AddKeyPageState extends State<AddKeyPage> {
  final name = TextEditingController(),
      pem = TextEditingController(),
      passphrase = TextEditingController();
  final form = GlobalKey<FormState>();
  String? error;
  bool busy = false;
  @override
  void dispose() {
    name.dispose();
    pem.dispose();
    passphrase.dispose();
    super.dispose();
  }

  Future<void> file() async {
    try {
      final picked = await FilePicker.pickFile();
      if (picked == null || !mounted) return;
      final bytes = await picked.readAsBytes();
      if (!mounted) return;
      pem.text = utf8.decode(bytes);
      if (name.text.isEmpty) name.text = picked.name;
    } catch (e) {
      if (mounted) setState(() => error = '$e');
    }
  }

  Future<void> save() async {
    if (!form.currentState!.validate()) return;
    setState(() {
      busy = true;
      error = null;
    });
    try {
      final store = IdentityStore();
      final key = widget.generate
          ? await store.generate(name.text.trim())
          : await store.import(name.text.trim(), pem.text, passphrase.text);
      if (mounted) Navigator.pop(context, key);
    } catch (e) {
      if (mounted) {
        setState(() {
          busy = false;
          error = '$e';
        });
      }
    }
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(
      title: Text(widget.generate ? 'Generate key' : 'Import key'),
    ),
    body: Form(
      key: form,
      child: ListView(
        padding: const EdgeInsets.all(20),
        children: [
          TextFormField(
            controller: name,
            decoration: const InputDecoration(
              labelText: 'Key name',
              hintText: 'Phone',
            ),
            validator: (v) => v!.trim().isEmpty ? 'Enter a name' : null,
          ),
          const SizedBox(height: 20),
          if (widget.generate)
            const Card(
              child: ListTile(
                leading: Icon(Icons.verified_user_outlined),
                title: Text('Ed25519'),
                subtitle: Text(
                  'After generating, copy the public key to your server.',
                ),
              ),
            ),
          if (!widget.generate) ...[
            OutlinedButton.icon(
              onPressed: busy ? null : file,
              icon: const Icon(Icons.folder_open),
              label: const Text('Choose key file'),
            ),
            const SizedBox(height: 16),
            TextFormField(
              controller: pem,
              minLines: 5,
              maxLines: 8,
              style: mono,
              autocorrect: false,
              enableSuggestions: false,
              decoration: InputDecoration(
                labelText: 'Private key',
                hintText: 'Or paste a PEM / OpenSSH private key',
                suffixIcon: QrScanButton(
                  controller: pem,
                  label: 'private key',
                  enabled: !busy,
                ),
              ),
              validator: (v) => v!.trim().isEmpty
                  ? 'Choose a file, paste, or scan a private key'
                  : null,
            ),
            const SizedBox(height: 16),
            TextFormField(
              controller: passphrase,
              obscureText: true,
              autocorrect: false,
              enableSuggestions: false,
              decoration: InputDecoration(
                labelText: 'Passphrase (if encrypted)',
                suffixIcon: QrScanButton(
                  controller: passphrase,
                  label: 'passphrase',
                  enabled: !busy,
                ),
              ),
            ),
          ],
          if (error != null) ...[const SizedBox(height: 16), ErrorBox(error!)],
          const SizedBox(height: 24),
          FilledButton(
            onPressed: busy ? null : save,
            child: Text(
              busy
                  ? 'Saving…'
                  : widget.generate
                  ? 'Generate Ed25519 key'
                  : 'Import key',
            ),
          ),
        ],
      ),
    ),
  );
}
