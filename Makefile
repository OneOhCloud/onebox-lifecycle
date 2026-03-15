BINARY   := demo_full
DEBUG    := target/debug/examples/$(BINARY)
RELEASE  := target/release/examples/$(BINARY)
LOG      := onebox_lifecycle_demo.log
PLIST_ID := com.onebox.lifecycle.demo

.PHONY: all debug release run run-detached stop install-agent uninstall-agent log clean

all: debug

# ── Build ─────────────────────────────────────────────────────────────────────

debug:
	cargo build --example $(BINARY)
	cp $(DEBUG) .

release:
	cargo build --release --example $(BINARY)
	cp $(RELEASE) .

# ── Run ───────────────────────────────────────────────────────────────────────

# Run in foreground (Ctrl-C exits; shutdown test limited by Terminal interference)
run: debug
	./$(BINARY)

# Run detached from Terminal — required to test shutdown blocking correctly.
#
# When running inside Terminal.app, macOS asks Terminal to quit FIRST during
# shutdown. Terminal then kills its child processes (including this demo) before
# they can call replyToApplicationShouldTerminate:, so shutdown gets permanently
# stuck. Running detached breaks this parent-child relationship.
#
# After this: use 'make log' to watch events, then trigger shutdown/sleep/network.
run-detached: debug
	@nohup ./$(BINARY) >> $(LOG) 2>&1 & \
	 PID=$$! ; disown $$PID ; echo $$PID > .demo_pid ; \
	 echo "Started PID=$$PID  log=$(LOG)"
	@echo "Tip: 'make log' to tail, 'make stop' to kill"

stop:
	@if [ -f .demo_pid ]; then \
	  PID=$$(cat .demo_pid) ; \
	  kill $$PID 2>/dev/null && echo "Stopped PID=$$PID" || echo "PID=$$PID not found" ; \
	  rm -f .demo_pid ; \
	else \
	  pkill -f ./$(BINARY) && echo "Stopped." || echo "Not running." ; \
	fi

# ── launchd agent — best way to test shutdown blocking ───────────────────────
#
# A LaunchAgent is NOT a child of Terminal, runs at login, and receives
# applicationShouldTerminate: cleanly during system shutdown/restart.

install-agent: release
	@BINARY_PATH="$$(pwd)/$(BINARY)" ; \
	 LOG_PATH="$$(pwd)/$(LOG)" ; \
	 PLIST=~/Library/LaunchAgents/$(PLIST_ID).plist ; \
	 printf '<?xml version="1.0" encoding="UTF-8"?>\n\
	<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n\
	<plist version="1.0"><dict>\n\
	  <key>Label</key><string>$(PLIST_ID)</string>\n\
	  <key>ProgramArguments</key><array><string>'"$$BINARY_PATH"'</string></array>\n\
	  <key>StandardOutPath</key><string>'"$$LOG_PATH"'</string>\n\
	  <key>StandardErrorPath</key><string>'"$$LOG_PATH"'</string>\n\
	  <key>RunAtLoad</key><true/>\n\
	  <key>KeepAlive</key><false/>\n\
	</dict></plist>\n' > "$$PLIST" ; \
	 launchctl load "$$PLIST" ; \
	 echo "Agent loaded: $$PLIST" ; \
	 echo "Log: $$LOG_PATH"

uninstall-agent:
	@PLIST=~/Library/LaunchAgents/$(PLIST_ID).plist ; \
	 launchctl unload "$$PLIST" 2>/dev/null ; \
	 rm -f "$$PLIST" ; \
	 echo "Agent removed."

# ── Log ───────────────────────────────────────────────────────────────────────

log:
	tail -f $(LOG)

# ── Clean ─────────────────────────────────────────────────────────────────────

clean:
	cargo clean
	rm -f ./$(BINARY) $(LOG) .demo_pid
