from pathlib import Path
import requests
from datetime import datetime, timedelta
import sys

print(f"args: {sys.argv}")
# sys.exit(0)

client_id = "YOUR_CLIENT_ID"
client_secret = "YOUR_CLIENT_SECRET"

for line in Path(".twitch").read_text().splitlines():
    if line.startswith("CLIENT_ID="):
        client_id = line.split("=")[1]
    elif line.startswith("CLIENT_SECRET="):
        client_secret = line.split("=")[1]

username = sys.argv[1]
timestamp_str = sys.argv[2]
timestamp = 0
vod_date_time = None

try:
    vod_date_time = datetime.strptime(timestamp_str, "%Y-%m-%d %H:%M:%S") - timedelta(
        hours=10
    )
    timestamp = vod_date_time.timestamp()
except ValueError:
    try:
        timestamp = float(timestamp_str)
    except ValueError:
        print(f"Invalid timestamp format: {timestamp_str}")
        sys.exit(1)

# vod_date_time = datetime.fromtimestamp(timestamp)

print(f"Finding VOD for {username} at {vod_date_time}...")
print(f"Continue? [y/n]")
resp: str = input()
if resp.lower() != "y":
    print(f"Exiting... (expected `y`, got `{resp}`)")
    sys.exit(0)

# Get access token
token_url = "https://id.twitch.tv/oauth2/token"
params = {
    "client_id": client_id,
    "client_secret": client_secret,
    "grant_type": "client_credentials",
}

response = requests.post(token_url, params=params)
access_token = response.json()["access_token"]
print(f"Access token: {access_token}")

# Get user ID
user_url = f"https://api.twitch.tv/helix/users?login={username}"
headers = {"Client-ID": client_id, "Authorization": f"Bearer {access_token}"}

response = requests.get(user_url, headers=headers)
resp = response.json()
# print(f"User response: {resp}")
user_id = resp["data"][0]["id"]

# List VODs
vods_url = (
    f"https://api.twitch.tv/helix/videos?user_id={user_id}&type=archive&first=100"
)
response = requests.get(vods_url, headers=headers)
vods = response.json()["data"]
# print(f"VODs: {vods}")


def hms_to_timedelta(hms: str):
    """take a format like 3h33m36s and convert it to a timedelta"""
    if "h" not in hms:
        hms = f"0h{hms}"
    hms = hms.split("h")
    hours = int(hms[0])
    hms = hms[1].split("m")
    minutes = int(hms[0])
    hms = hms[1].split("s")
    seconds = int(hms[0])
    return timedelta(hours=hours, minutes=minutes, seconds=seconds)


def get_timestamped_link(vods, dt: datetime):
    for vod in vods:
        start_time = datetime.strptime(vod["created_at"], "%Y-%m-%dT%H:%M:%SZ")
        end_time = start_time + hms_to_timedelta(vod["duration"])
        print(f"VOD: {vod['url']} ({start_time} - {end_time})")

        if start_time <= dt <= end_time:
            seconds_since_start = int((dt - start_time).total_seconds())
            return f"{vod['url']}?t={seconds_since_start}s"

    return None


timestamped_link = get_timestamped_link(vods, vod_date_time)
print(timestamped_link)
