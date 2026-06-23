let currentOverrideMac = '';

// Helper to format timestamps
function formatTime(timestampString) {
    if (!timestampString) return '';
    try {
        // SystemTime serialises to e.g. {"secs_since_epoch":12345678,"nanos_since_epoch":0}
        let secs = timestampString.secs_since_epoch;
        if (secs) {
            return new Date(secs * 1000).toLocaleTimeString();
        }
        return new Date(timestampString).toLocaleTimeString();
    } catch (e) {
        return '';
    }
}

// Fetch and update status
async function updateStatus() {
    try {
        const response = await fetch('/api/status');
        if (!response.ok) throw new Error('Network error');
        
        const data = await response.json();
        const hostsList = document.getElementById('hosts-list');
        const hostCount = document.getElementById('host-count');
        const select = document.getElementById('override-target');
        
        // 1. Update host count badge
        const hosts = data.hosts || [];
        hostCount.textContent = `${hosts.len || hosts.length} Host(s)`;

        // 2. Populate available targets in modal dropdown if not already populated
        const configs = data.configs || [];
        const currentOptionsCount = select.options.length;
        if (currentOptionsCount <= 1) {
            select.innerHTML = '<option value="" disabled selected>-- Select boot configuration --</option>';
            configs.forEach(cfg => {
                const opt = document.createElement('option');
                opt.value = cfg.name;
                opt.textContent = `${cfg.name} (${cfg.mac})`;
                select.appendChild(opt);
            });
        }

        // 3. Render host rows
        if (hosts.length === 0) {
            hostsList.innerHTML = `
                <tr class="empty-row">
                    <td colspan="5">Scanning network for PXE boot requests...</td>
                </tr>
            `;
            return;
        }

        let html = '';
        hosts.forEach(host => {
            const mac = host.mac;
            const name = host.name || 'Unrecognized Host';
            const ip = host.client_ip || 'No Lease IP';
            const arch = host.architecture || 'Unknown';
            const status = (host.status || 'polling').toLowerCase();
            const target = host.assigned_target ? ` &rarr; ${host.assigned_target}` : '';
            
            let statusClass = 'polling';
            if (status === 'booting') statusClass = 'booting';
            if (status === 'completed') statusClass = 'completed';
            if (status === 'failed') statusClass = 'failed';

            html += `
                <tr>
                    <td><span class="mac-cell">${mac}</span></td>
                    <td>
                        <div class="host-info">
                            <span class="host-name">${name}${target}</span>
                            <span class="host-ip">${ip}</span>
                        </div>
                    </td>
                    <td>${arch}</td>
                    <td><span class="status-badge ${statusClass}">${status}</span></td>
                    <td>
                        <button class="btn btn-primary btn-sm" onclick="openOverrideModal('${mac}')">
                            Assign Target
                        </button>
                    </td>
                </tr>
            `;
        });
        hostsList.innerHTML = html;
    } catch (err) {
        console.error('Failed to fetch status:', err);
    }
}

// Fetch and update logs
async function updateLogs() {
    try {
        const response = await fetch('/api/logs');
        if (!response.ok) throw new Error('Network error');
        
        const logs = await response.json();
        const logsList = document.getElementById('logs-list');
        
        if (logs.length === 0) {
            logsList.innerHTML = '<div class="log-entry system">No events logged yet.</div>';
            return;
        }

        let html = '';
        // Show newest logs at top or bottom? Standard is showing newest at top, but list logs has them chronologically.
        // Let's reverse them so the latest logs are right at the top!
        const reversedLogs = [...logs].reverse();
        reversedLogs.forEach(entry => {
            const time = formatTime(entry.timestamp);
            const level = (entry.level || 'info').toLowerCase();
            const mac = entry.mac ? `<span class="log-mac">[${entry.mac}]</span>` : '';
            const msg = entry.message;
            
            html += `
                <div class="log-entry ${level}">
                    <span class="log-time">${time}</span>
                    ${mac}
                    <span class="log-message">${msg}</span>
                </div>
            `;
        });
        logsList.innerHTML = html;
    } catch (err) {
        console.error('Failed to fetch logs:', err);
    }
}

// Modal handling
window.openOverrideModal = function(mac) {
    currentOverrideMac = mac;
    document.getElementById('modal-mac').textContent = mac;
    document.getElementById('override-modal').classList.add('active');
};

function closeModal() {
    document.getElementById('override-modal').classList.remove('active');
    document.getElementById('override-form').reset();
}

document.getElementById('close-modal-btn').addEventListener('click', closeModal);
document.getElementById('cancel-override-btn').addEventListener('click', closeModal);

// Handle manual override form submission
document.getElementById('override-form').addEventListener('submit', async (e) => {
    e.preventDefault();
    const targetSelect = document.getElementById('override-target');
    const target = targetSelect.value;

    if (!currentOverrideMac || !target) return;

    try {
        const response = await fetch('/api/override', {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json'
            },
            body: JSON.stringify({
                mac: currentOverrideMac,
                target: target
            })
        });

        if (response.ok) {
            closeModal();
            updateStatus();
            updateLogs();
        } else {
            alert('Failed to assign target configuration.');
        }
    } catch (err) {
        console.error('Error submitting override:', err);
    }
});

// Clear logs view handler (local UI clear)
document.getElementById('clear-logs-btn').addEventListener('click', () => {
    document.getElementById('logs-list').innerHTML = '<div class="log-entry system">View cleared. Logging resumes...</div>';
});

// Initialise polling
updateStatus();
updateLogs();
setInterval(updateStatus, 2500);
setInterval(updateLogs, 2500);
window.addEventListener('DOMContentLoaded', () => {
    // Keep local clock running in header
    const serverTimeEl = document.getElementById('server-time');
    setInterval(() => {
        serverTimeEl.textContent = `Local Time: ${new Date().toLocaleTimeString()}`;
    }, 1000);
});
