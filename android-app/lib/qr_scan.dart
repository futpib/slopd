import 'package:flutter/material.dart';
import 'package:flutter_zxing/flutter_zxing.dart';

class QrScanButton extends StatefulWidget {
  const QrScanButton({
    super.key,
    required this.controller,
    required this.label,
    this.enabled = true,
    this.trim = false,
  });

  final TextEditingController controller;
  final String label;
  final bool enabled, trim;

  @override
  State<QrScanButton> createState() => _QrScanButtonState();
}

class _QrScanButtonState extends State<QrScanButton> {
  Future<void> scan() async {
    FocusScope.of(context).unfocus();
    final result = await Navigator.of(context).push<String>(
      MaterialPageRoute(builder: (_) => QrScanPage(label: widget.label)),
    );
    if (!mounted || result == null || !widget.enabled) return;
    final text = widget.trim ? result.trim() : result;
    widget.controller.value = TextEditingValue(
      text: text,
      selection: TextSelection.collapsed(offset: text.length),
    );
  }

  @override
  Widget build(BuildContext context) => IconButton(
    tooltip: 'Scan ${widget.label} QR code',
    onPressed: widget.enabled ? scan : null,
    icon: const Icon(Icons.qr_code_scanner),
  );
}

class QrScanPage extends StatefulWidget {
  const QrScanPage({super.key, required this.label});
  final String label;

  @override
  State<QrScanPage> createState() => _QrScanPageState();
}

class _QrScanPageState extends State<QrScanPage> {
  bool scanned = false, cameraUnavailable = false;

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(title: Text('Scan ${widget.label}')),
    body: Stack(
      children: [
        ReaderWidget(
          codeFormat: Format.qrCode,
          tryHarder: true,
          resolution: ResolutionPreset.veryHigh,
          cropPercent: 0,
          galleryIcon: const Tooltip(
            message: 'Choose QR image',
            child: Icon(Icons.photo_library),
          ),
          onScan: (result) {
            final text = result.text;
            if (!mounted ||
                scanned ||
                !result.isValid ||
                text == null ||
                text.isEmpty) {
              return;
            }
            scanned = true;
            Navigator.of(context).pop(text);
          },
          onControllerCreated: (_, error) {
            if (mounted) setState(() => cameraUnavailable = error != null);
          },
        ),
        if (cameraUnavailable)
          const Positioned(
            top: 16,
            left: 16,
            right: 16,
            child: Card(
              child: Padding(
                padding: EdgeInsets.all(16),
                child: Text(
                  'Camera unavailable. Allow camera access in Android settings, or choose a QR image below.',
                ),
              ),
            ),
          ),
      ],
    ),
  );
}
