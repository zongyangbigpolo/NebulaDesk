import 'package:flutter/material.dart';

import 'cloud_session_controller.dart';
import 'cloud_tab.dart';
import 'host_tab.dart';
import 'models.dart';
import 'session_launcher.dart';
import 'store.dart';
import 'vda_launcher.dart';

void main() {
  runApp(const NebulaManagerApp());
}

class NebulaManagerApp extends StatelessWidget {
  const NebulaManagerApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Nebula Manager',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(
        colorSchemeSeed: Colors.indigo,
        useMaterial3: true,
        brightness: Brightness.dark,
      ),
      home: const HomePage(),
    );
  }
}

class HomePage extends StatefulWidget {
  const HomePage({super.key});
  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> with SingleTickerProviderStateMixin {
  late final TabController _tabController;
  final _store = VdaStore();
  final _launcher = SessionLauncher();
  final _cloudLauncher = SessionLauncher();
  final _vdaLauncher = VdaLauncher();
  // Shared by the "Cloud" and "Host this Mac" tabs — one login, two
  // capabilities (viewing accessible devices vs. hosting this Mac as one).
  final _cloudSession = CloudSessionController();
  final List<VdaEntry> _vdas = [];
  // vda index -> live session state / pid
  final Map<int, SessionState> _states = {};
  final Map<int, int> _pids = {};

  @override
  void initState() {
    super.initState();
    _tabController = TabController(length: 3, vsync: this);
    _load();
  }

  Future<void> _load() async {
    final list = await _store.load();
    setState(() {
      _vdas
        ..clear()
        ..addAll(list);
    });
  }

  Future<void> _persist() => _store.save(_vdas);

  Future<void> _connect(int index) async {
    final vda = _vdas[index];
    setState(() => _states[index] = SessionState.connecting);
    try {
      final proc = await _launcher.launch(
        vda,
        onState: (s) => setState(() => _states[index] = s),
        onExit: (_) => setState(() {
          _states[index] = SessionState.disconnected;
          _pids.remove(index);
        }),
      );
      setState(() => _pids[index] = proc.pid);
    } catch (e) {
      setState(() => _states[index] = SessionState.error);
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(content: Text('Failed to launch session: $e')),
        );
      }
    }
  }

  void _disconnect(int index) {
    final pid = _pids[index];
    if (pid != null) _launcher.terminate(pid);
  }

  Future<void> _addOrEdit({int? index}) async {
    final result = await showDialog<VdaEntry>(
      context: context,
      builder: (_) => VdaDialog(entry: index != null ? _vdas[index] : null),
    );
    if (result == null) return;
    setState(() {
      if (index != null) {
        _vdas[index] = result;
      } else {
        _vdas.add(result);
      }
    });
    await _persist();
  }

  Future<void> _delete(int index) async {
    setState(() => _vdas.removeAt(index));
    await _persist();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('Nebula — Connections'),
        bottom: TabBar(
          controller: _tabController,
          tabs: const [
            Tab(text: 'Direct', icon: Icon(Icons.link)),
            Tab(text: 'Cloud', icon: Icon(Icons.cloud_outlined)),
            Tab(text: 'Host this Mac', icon: Icon(Icons.podcasts)),
          ],
        ),
        actions: [
          AnimatedBuilder(
            animation: _tabController,
            builder: (_, _) => _tabController.index == 0
                ? IconButton(
                    icon: const Icon(Icons.add),
                    tooltip: 'Add VDA',
                    onPressed: () => _addOrEdit(),
                  )
                : const SizedBox.shrink(),
          ),
        ],
      ),
      body: TabBarView(
        controller: _tabController,
        children: [
          _vdas.isEmpty
              ? const Center(child: Text('No VDAs yet. Tap + to add one.'))
              : ListView.separated(
                  itemCount: _vdas.length,
                  separatorBuilder: (_, _) => const Divider(height: 1),
                  itemBuilder: (_, i) => _vdaTile(i),
                ),
          CloudTab(controller: _cloudSession, launcher: _cloudLauncher),
          HostTab(controller: _cloudSession, launcher: _vdaLauncher),
        ],
      ),
    );
  }

  Widget _vdaTile(int i) {
    final vda = _vdas[i];
    final state = _states[i] ?? SessionState.idle;
    final connected = _pids.containsKey(i);
    return ListTile(
      leading: _stateIcon(state),
      title: Text(vda.name),
      subtitle: Text(
        '${vda.host}:${vda.port}'
        '${vda.relayHost.isNotEmpty ? '  •  relay: ${vda.relayHost}:${vda.relayPort}' : '  •  direct'}'
        '  •  ${state.name}',
      ),
      trailing: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (!connected)
            FilledButton.icon(
              icon: const Icon(Icons.play_arrow),
              label: const Text('Connect'),
              onPressed: () => _connect(i),
            )
          else
            OutlinedButton.icon(
              icon: const Icon(Icons.stop),
              label: const Text('Disconnect'),
              onPressed: () => _disconnect(i),
            ),
          PopupMenuButton<String>(
            onSelected: (v) {
              if (v == 'edit') _addOrEdit(index: i);
              if (v == 'delete') _delete(i);
            },
            itemBuilder: (_) => const [
              PopupMenuItem(value: 'edit', child: Text('Edit')),
              PopupMenuItem(value: 'delete', child: Text('Delete')),
            ],
          ),
        ],
      ),
    );
  }

  Widget _stateIcon(SessionState s) {
    switch (s) {
      case SessionState.streaming:
      case SessionState.direct:
        return const Icon(Icons.videocam, color: Colors.greenAccent);
      case SessionState.connected:
        return const Icon(Icons.link, color: Colors.lightBlueAccent);
      case SessionState.connecting:
        return const SizedBox(
            width: 20, height: 20, child: CircularProgressIndicator(strokeWidth: 2));
      case SessionState.error:
        return const Icon(Icons.error, color: Colors.redAccent);
      case SessionState.disconnected:
      case SessionState.idle:
        return const Icon(Icons.desktop_windows_outlined);
    }
  }

  @override
  void dispose() {
    _tabController.dispose();
    _launcher.terminateAll();
    _cloudLauncher.terminateAll();
    _vdaLauncher.stop();
    _cloudSession.dispose();
    super.dispose();
  }
}

// Add/edit dialog for a VDA entry.
class VdaDialog extends StatefulWidget {
  final VdaEntry? entry;
  const VdaDialog({super.key, this.entry});
  @override
  State<VdaDialog> createState() => _VdaDialogState();
}

class _VdaDialogState extends State<VdaDialog> {
  late final TextEditingController _name;
  late final TextEditingController _host;
  late final TextEditingController _port;
  late final TextEditingController _relay;
  late final TextEditingController _relayPort;
  late final TextEditingController _deviceId;
  late final TextEditingController _token;
  late final TextEditingController _secret;

  @override
  void initState() {
    super.initState();
    final e = widget.entry;
    _name = TextEditingController(text: e?.name ?? '');
    _host = TextEditingController(text: e?.host ?? '');
    _port = TextEditingController(text: (e?.port ?? 7000).toString());
    _relay = TextEditingController(text: e?.relayHost ?? '');
    _relayPort = TextEditingController(text: (e?.relayPort ?? 7100).toString());
    _deviceId = TextEditingController(text: e?.deviceId ?? '');
    _token = TextEditingController(text: e?.token ?? '');
    _secret = TextEditingController(text: e?.secret ?? 'nebula-default-psk');
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: Text(widget.entry == null ? 'Add VDA' : 'Edit VDA'),
      content: SizedBox(
        width: 380,
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            _field(_name, 'Name', 'My Mac'),
            _field(_host, 'Host (IP or hostname)', '192.168.1.50'),
            _field(_port, 'Port', '7000', number: true),
            _field(_relay, 'Relay host (optional, blank = direct)', 'relay.example.com'),
            _field(_relayPort, 'Relay port', '7100', number: true),
            _field(_deviceId, 'Relay device ID', 'my-mac'),
            _field(_token, 'Relay pairing token', '', obscure: true),
            _field(_secret, 'Shared secret', 'nebula-default-psk', obscure: true),
          ],
        ),
      ),
      actions: [
        TextButton(onPressed: () => Navigator.pop(context), child: const Text('Cancel')),
        FilledButton(
          onPressed: () {
            final entry = VdaEntry(
              name: _name.text.trim().isEmpty ? _host.text.trim() : _name.text.trim(),
              host: _host.text.trim(),
              port: int.tryParse(_port.text.trim()) ?? 7000,
              relayHost: _relay.text.trim(),
              relayPort: int.tryParse(_relayPort.text.trim()) ?? 7100,
              deviceId: _deviceId.text.trim(),
              token: _token.text,
              secret: _secret.text.isEmpty ? 'nebula-default-psk' : _secret.text,
            );
            Navigator.pop(context, entry);
          },
          child: const Text('Save'),
        ),
      ],
    );
  }

  Widget _field(TextEditingController c, String label, String hint,
      {bool number = false, bool obscure = false}) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 6),
      child: TextField(
        controller: c,
        obscureText: obscure,
        keyboardType: number ? TextInputType.number : TextInputType.text,
        decoration: InputDecoration(
          labelText: label,
          hintText: hint,
          border: const OutlineInputBorder(),
        ),
      ),
    );
  }
}
