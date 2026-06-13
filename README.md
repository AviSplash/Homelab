# AviSplash's Homelab
<HTML>
  <H>
    Welcome to my homelab where I show off what I run at home, follow issues found and documented fixes.  
  </H>
  <body>
    Lets start off by saying I have been working in IT as of 2/2/2024. I intitally started working at a cellphone repair store, after about a year, I was able to "break through" to "real" IT. My technical job title is Field Technician, however, I preform system administration for 10-30 clients, all having over 1000+ end users. Most enviroments I work with are Microsoft tenants with a local infrastructure (AD, DNS, DHCP, etc) all the way to cloud based tenants using Google and Macs. 

6/12/2026 
  Recently I have been working with Claude to start a side hustle named "Workshopsites.com" we focus on building "vibe coded" websites to small and blue collar businesses. As of writing this. I am working on development as work is too busy to cold call consistently. 

  n8n Local Network Access — What We Did
The Goal
Run n8n via npx n8n on one Windows machine and access it from other devices on the same LAN.

What We Did (In Order)
1. Bound n8n to all network interfaces

By default, n8n only listens on localhost (127.0.0.1), meaning no other machine can reach it. Setting N8N_HOST=0.0.0.0 tells n8n to listen on every network interface, making it reachable by LAN IP.
2. Killed a stuck n8n process

Port 5678 was already occupied by a hung node.exe process from a previous session. Used netstat -ano | findstr :5678 to find the PID, then taskkill /PID <PID> /F to kill it.
3. Added a Windows Firewall inbound rule

Windows blocks unsolicited inbound traffic by default. Created a rule to allow TCP on port 5678 using New-NetFirewallRule in an elevated (Administrator) PowerShell window. Running it in a non-admin window gave "Access is denied."
4. Corrected the LAN IP

The environment variables were initially set with 192.168.1.105 but the host machine's actual IP was 192.168.1.229. This caused n8n to point its internal WEBHOOK_URL at the wrong machine, and the client had no valid address to connect to. Confirmed the real IP via netstat output.
5. Disabled the secure cookie requirement

n8n defaults to N8N_SECURE_COOKIE=true, which requires HTTPS. Since the LAN is running plain HTTP, the browser refused to store the session cookie and blocked login. Setting N8N_SECURE_COOKIE=false resolves this for local HTTP access.

The Final Working Startup Command
powershell$env:N8N_HOST="0.0.0.0"
$env:N8N_PORT="5678"
$env:WEBHOOK_URL="http://192.168.1.229:5678/"
$env:N8N_EDITOR_BASE_URL="http://192.168.1.229:5678/"
$env:N8N_SECURE_COOKIE="false"
npx n8n

Root Causes Summary
ProblemCauseFixNo LAN accessn8n bound to localhost onlyN8N_HOST=0.0.0.0Port already in useOrphaned node.exe from prior sessiontaskkill /PIDFirewall blockingWindows default blocks inboundAdded firewall rule as AdminConnection timeoutWrong LAN IP in env variablesCorrected to 192.168.1.229Cookie/login errorSecure cookie requires HTTPSN8N_SECURE_COOKIE=false

One thing to remember going forward: always stop n8n with Ctrl+C rather than closing the terminal window. Closing the window orphans the Node process and you'll hit the "port already in use" error next time.
  </body>
  
</HTML>
