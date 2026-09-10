import AppKit

final class Document: NSObject, NSWindowDelegate, NSTextFieldDelegate {
    let id: Int
    let window: NSWindow
    let field = NSTextField()
    let counter = NSTextField(labelWithString: "")
    let resetButton = NSButton(title: "Reset fixture", target: nil, action: nil)
    var tick = 0
    var dirty = false
    var closeRequests = 0
    var cancelledCloses = 0
    var resetPresses = 0
    var timer: Timer?
    weak var owner: Fixture?

    init(id: Int, owner: Fixture) {
        self.id = id
        self.owner = owner
        window = NSWindow(
            contentRect: NSRect(x: 80 + id * 50, y: 180 + id * 35, width: 600, height: 380),
            styleMask: [.titled, .closable, .resizable, .miniaturizable],
            backing: .buffered, defer: false)
        super.init()
        resetButton.target = self
        resetButton.action = #selector(resetFixture)
        window.title = "Nebula acceptance document \(id)"
        window.isReleasedWhenClosed = false
        window.delegate = self
        let content = NSView(frame: NSRect(x: 0, y: 0, width: 600, height: 380))
        content.wantsLayer = true
        content.layer?.backgroundColor = (id % 2 == 0 ? NSColor.systemBlue : NSColor.systemOrange)
            .withAlphaComponent(0.25).cgColor
        counter.frame = NSRect(x: 24, y: 270, width: 540, height: 60)
        counter.font = .monospacedSystemFont(ofSize: 26, weight: .bold)
        counter.autoresizingMask = [.width, .minYMargin]
        field.frame = NSRect(x: 24, y: 200, width: 540, height: 40)
        field.placeholderString = "Type through the remote application window"
        field.delegate = self
        resetButton.frame = NSRect(x: 24, y: 120, width: 180, height: 32)
        resetButton.autoresizingMask = [.minYMargin]
        content.addSubview(resetButton)
        field.autoresizingMask = [.width, .minYMargin]
        content.addSubview(counter)
        content.addSubview(field)
        window.contentView = content
        timer = Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.tick += 1
            self.counter.stringValue = "Document \(self.id) - tick \(self.tick)"
            if self.tick % 10 == 0 { self.owner?.report() }
        }
        window.makeKeyAndOrderFront(nil)
    }

    func controlTextDidChange(_ notification: Notification) {
        dirty = true
        owner?.report()
    }

    @objc func resetFixture() {
        resetPresses += 1
        field.stringValue = ""
        dirty = false
        owner?.report()
    }

    var resetPoint: [String: Double] {
        let rect = window.convertToScreen(resetButton.convert(resetButton.bounds, to: nil))
        return [
            "x": (rect.midX - window.frame.minX) / window.frame.width,
            "y": (window.frame.maxY - rect.midY) / window.frame.height
        ]
    }

    func windowDidResize(_ notification: Notification) { owner?.report() }
    func windowDidMiniaturize(_ notification: Notification) { owner?.report() }
    func windowDidDeminiaturize(_ notification: Notification) { owner?.report() }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        closeRequests += 1
        guard dirty else { return true }
        guard window.attachedSheet == nil else { return false }
        let alert = NSAlert()
        alert.messageText = "Keep changes to document \(id)?"
        alert.informativeText = "Cancel must keep this application window connected."
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Discard")
        alert.beginSheetModal(for: window) { [weak self] result in
            guard let self else { return }
            if result == .alertSecondButtonReturn {
                self.dirty = false
                self.window.close()
            } else {
                self.cancelledCloses += 1
                self.owner?.report()
            }
        }
        owner?.report()
        return false
    }

    func windowWillClose(_ notification: Notification) {
        timer?.invalidate()
        owner?.documents.removeValue(forKey: id)
        owner?.report()
    }
}

final class Fixture: NSObject, NSApplicationDelegate {
    var documents: [Int: Document] = [:]
    var nextID = 1
    var keyDownCodes: [Int] = []
    var keyUpCodes: [Int] = []
    var mouseEvents: [[String: Any]] = []
    var inputMonitor: Any?
    let statusDirectory = URL(fileURLWithPath:
        Bundle.main.object(forInfoDictionaryKey: "NebulaFixtureStatusDirectory") as? String ?? "/tmp")
    lazy var status = statusDirectory.appendingPathComponent(
        "nebula-seamless-fixture-status-\(ProcessInfo.processInfo.processIdentifier).json")
    lazy var latestStatus = statusDirectory.appendingPathComponent("nebula-seamless-fixture-status.json")

    func applicationDidFinishLaunching(_ notification: Notification) {
        inputMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp, .leftMouseDown, .leftMouseUp]) { [weak self] event in
            if let self {
                if event.type == .keyDown {
                    self.keyDownCodes.append(Int(event.keyCode))
                    if self.keyDownCodes.count > 128 { self.keyDownCodes.removeFirst() }
                } else if event.type == .keyUp {
                    self.keyUpCodes.append(Int(event.keyCode))
                    if self.keyUpCodes.count > 128 { self.keyUpCodes.removeFirst() }
                } else {
                    let document = self.documents.values.first { $0.window.windowNumber == event.windowNumber }
                    let point = event.locationInWindow
                    var diagnostic: [String: Any] = [
                        "type": event.type.rawValue,
                        "window_number": event.windowNumber,
                        "window_resolved": event.window != nil,
                        "local_x": point.x, "local_y": point.y,
                        "button": event.buttonNumber,
                        "click_count": event.clickCount,
                        "pressed_buttons": NSEvent.pressedMouseButtons
                    ]
                    if let cg = event.cgEvent {
                        diagnostic["cg_x"] = cg.location.x
                        diagnostic["cg_y"] = cg.location.y
                    }
                    if let document {
                        let rect = document.resetButton.convert(document.resetButton.bounds, to: nil)
                        diagnostic["document_id"] = document.id
                        diagnostic["reset_rect"] = [
                            "x": rect.minX, "y": rect.minY, "width": rect.width, "height": rect.height
                        ]
                        diagnostic["inside_reset"] = rect.contains(point)
                    }
                    self.mouseEvents.append(diagnostic)
                    if self.mouseEvents.count > 32 { self.mouseEvents.removeFirst() }
                }
                self.report()
            }
            return event
        }
        let menu = NSMenu()
        let application = NSMenuItem()
        application.submenu = NSMenu()
        application.submenu?.addItem(
            withTitle: "Quit fixture", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        menu.addItem(application)
        let file = NSMenuItem(title: "File", action: nil, keyEquivalent: "")
        file.submenu = NSMenu(title: "File")
        let new = file.submenu!.addItem(
            withTitle: "New Document", action: #selector(newDocument), keyEquivalent: "n")
        new.target = self
        file.submenu?.addItem(
            withTitle: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        menu.addItem(file)
        let edit = NSMenuItem(title: "Edit", action: nil, keyEquivalent: "")
        edit.submenu = NSMenu(title: "Edit")
        for (title, selector, key) in [
            ("Select All", "selectAll:", "a"),
            ("Copy", "copy:", "c"),
            ("Paste", "paste:", "v")
        ] {
            edit.submenu?.addItem(withTitle: title, action: NSSelectorFromString(selector), keyEquivalent: key)
        }
        menu.addItem(edit)
        NSApplication.shared.mainMenu = menu
        newDocument()
        if !CommandLine.arguments.contains("--one-window") { newDocument() }
        if CommandLine.arguments.contains("--dirty-first"), let first = documents[1] {
            first.field.stringValue = "unsaved fixture text"
            first.dirty = true
        }
        report()
        NSApplication.shared.activate(ignoringOtherApps: true)
    }

    @objc func newDocument() {
        let id = nextID
        nextID += 1
        documents[id] = Document(id: id, owner: self)
        report()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    func report() {
        let values: [String: Any] = [
            "pid": ProcessInfo.processInfo.processIdentifier,
            "key_down_codes": keyDownCodes,
            "key_up_codes": keyUpCodes,
            "mouse_events": mouseEvents,
            "windows": documents.values.sorted { $0.id < $1.id }.map {
                [
                    "id": $0.id, "title": $0.window.title, "tick": $0.tick,
                    "window_number": $0.window.windowNumber,
                    "text": $0.field.stringValue, "dirty": $0.dirty,
                    "width": $0.window.frame.width, "height": $0.window.frame.height,
                    "miniaturized": $0.window.isMiniaturized,
                    "sheet": $0.window.attachedSheet != nil,
                    "close_requests": $0.closeRequests, "cancelled_closes": $0.cancelledCloses,
                    "reset_presses": $0.resetPresses, "reset_button": $0.resetPoint
                ] as [String: Any]
            }
        ]
        do {
            let data = try JSONSerialization.data(withJSONObject: values, options: [.sortedKeys])
            try data.write(to: status, options: .atomic)
            try data.write(to: latestStatus, options: .atomic)
        } catch {
            fputs("Fixture status write failed: \(error)\n", stderr)
        }
    }
}

let application = NSApplication.shared
let fixture = Fixture()
application.setActivationPolicy(.regular)
application.delegate = fixture
application.run()
