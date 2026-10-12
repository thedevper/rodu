# Rodu บน Windows: เข้าร่วมบอร์ดของทีม

[English](windows.md)

ทำครั้งเดียว ใช้เวลาราว 15 นาที เสร็จแล้วบอร์ดงานของทีมจะอยู่ในเครื่องเรา และ sync กับทีมผ่าน
Google Drive เอง ไม่ต้องมี server ไม่ต้องสมัครอะไรเพิ่ม

## ก่อนเริ่ม ต้องมี

- บัญชี Google ที่เจ้าของทีมแชร์โฟลเดอร์ของทีมให้แล้ว ด้วยสิทธิ์ **Editor**
- **invite** ที่เจ้าของทีมส่งให้ทางแชตส่วนตัว ใช้ได้ครั้งเดียว ภายใน 7 วัน
- PowerShell ซึ่งมีอยู่แล้วใน Windows: กด Start แล้วพิมพ์ `PowerShell`

## 1. ติดตั้ง Rodu

เปิด PowerShell (ไม่ต้อง Run as administrator) แล้ววางคำสั่งนี้

```powershell
irm https://raw.githubusercontent.com/TheDevper/rodu/v0.5.0/packaging/install.ps1 | iex
```

**ปิด PowerShell แล้วเปิดใหม่** จากนั้นเช็กว่าขึ้น `rodu 0.5.0`

```powershell
rodu --version
```

ถ้าใช้ [Scoop](https://scoop.sh) อยู่แล้ว ติดตั้งด้วย Scoop แทนได้:
`scoop bucket add thedevper https://github.com/TheDevper/scoop-bucket` แล้ว `scoop install rodu`

## 2. ติดตั้ง Google Drive for desktop

- ดาวน์โหลดจาก [google.com/drive/download](https://www.google.com/drive/download/) แล้วติดตั้ง
- ล็อกอินด้วยบัญชี Google ที่ได้รับแชร์โฟลเดอร์ของทีม
- เสร็จแล้วใน File Explorer จะมีไดรฟ์ **Google Drive (G:)** และข้างในมี `My Drive`

## 3. เอาโฟลเดอร์ทีมเข้า My Drive

โฟลเดอร์ที่คนอื่นแชร์มาจะยังไม่ลงมาที่เครื่อง จนกว่าเราจะเพิ่ม shortcut เข้า My Drive

- เปิด [drive.google.com → Shared with me](https://drive.google.com/drive/shared-with-me)
- คลิกขวาที่โฟลเดอร์ของทีม → **Organize → Add shortcut** → **My Drive** → **Add**
- รอสักครู่ แล้วเช็กว่าใน `G:\My Drive\` มีโฟลเดอร์นั้นแล้ว

## 4. เข้าร่วมทีม

สร้างโฟลเดอร์สำหรับบอร์ดในเครื่อง แล้วสั่ง join เมื่อ Rodu ถามหา invite ให้วางข้อความที่ได้รับมา
แล้วกด Enter

```powershell
mkdir $HOME\rodu-work
cd $HOME\rodu-work
rodu team join
```

Rodu จะหาโฟลเดอร์ทีมใน Google Drive เอง เสร็จแล้วบอกเจ้าของทีมว่า join แล้ว เครื่องของเขาจะรับ
เครื่องเราเข้าทีมเองตอน sync รอบถัดไป เช็กได้ด้วย

```powershell
rodu team members
```

ถ้า Rodu หา Google Drive ไม่เจอ ให้บอกโฟลเดอร์เอง:
`rodu team join --folder "G:\My Drive\<ชื่อโฟลเดอร์ทีม>"`

## 5. เปิดบอร์ด

```powershell
cd $HOME\rodu-work
rodu web
```

เบราว์เซอร์จะเปิดบอร์ดให้เอง ลากการ์ดข้ามคอลัมน์ได้ คลิกการ์ดเพื่อแก้หรือ comment
เปิดหน้าต่าง PowerShell ทิ้งไว้ระหว่างใช้ และกด `Ctrl+C` เมื่อเลิกใช้

ตอนนี้ทุกคำสั่ง `rodu` ต้องรันในโฟลเดอร์ `rodu-work` (หรือโฟลเดอร์ย่อยข้างใน)

## 6. ต่อกับ Claude Code (ถ้าใช้)

```powershell
cd $HOME\rodu-work
claude mcp add rodu -- rodu mcp
```

เปิด `claude` ในโฟลเดอร์นี้ แล้วสั่งได้เลย เช่น "ดูงานของฉันใน rodu" หรือ "สร้างการ์ด bug:
ปุ่ม login ไม่ทำงาน" สิ่งที่ agent แก้จะบันทึกว่าเป็นชื่อเรา via agent

## คำสั่งที่ใช้บ่อย

| คำสั่ง | ทำอะไร |
|---|---|
| `rodu web` | เปิดบอร์ดในเบราว์เซอร์ |
| `rodu add แก้หน้า login --type bug` | สร้างการ์ด |
| `rodu ls "assignee = me()"` | ดูงานของเรา |
| `rodu mv DEMO-12 "In Progress"` | ย้ายการ์ด |
| `rodu sync` | sync เดี๋ยวนี้ (ปกติทุกคำสั่ง sync เองอยู่แล้ว) |
| `rodu team` | ดูสถานะทีมของเครื่องนี้ |

การ์ดที่เราสร้างก่อนเครื่องเจ้าของทีม sync จะได้ key ชั่วคราว เช่น `DEMO-KQMRTZ` แล้วได้เลขจริง
ทีหลัง key ชั่วคราวยังใช้ได้

## อัปเดตเป็นเวอร์ชันใหม่

เมื่อมีเวอร์ชันใหม่ รันคำสั่งติดตั้งเดิมซ้ำได้เลย มันจะลงเวอร์ชันล่าสุดให้เอง

```powershell
irm https://raw.githubusercontent.com/TheDevper/rodu/v0.5.0/packaging/install.ps1 | iex
```

- อัปเดตได้แม้เปิด `rodu web` หรือ Claude Code ค้างไว้ แต่ต้องปิดแล้วเปิดใหม่ถึงจะได้เวอร์ชันใหม่:
  กด `Ctrl+C` ที่ `rodu web` แล้วรันใหม่ และเปิด Claude Code ใหม่
- เช็กด้วย `rodu --version` ข้อมูลบอร์ดและทีมอยู่ครบ ไม่ต้อง join ใหม่
- ถ้าลงผ่าน Scoop ใช้ `scoop update rodu` แทน ใช้วิธีติดตั้งแบบเดียว ไม่ต้องลงทั้งสองแบบ

## ข้อควรรู้

> **invite คือกุญแจของทีม** ใช้เปิดอ่านบอร์ดได้ เก็บเหมือนรหัสผ่าน ห้ามโพสต์ในกลุ่มหรือส่งต่อให้คนอื่น

- เปิด Google Drive for desktop ทิ้งไว้ ถ้าปิด งานของเราจะไม่ไปถึงทีม (บอร์ดในเครื่องยังใช้ได้ปกติ)
- ห้ามแก้ ลบ ย้าย หรือเปลี่ยนชื่อไฟล์ในโฟลเดอร์ทีมบน Drive เอง ไฟล์เหล่านั้นเข้ารหัสไว้และ Rodu
  จัดการเอง
- งานจากเพื่อนจะขึ้นภายในไม่กี่วินาทีถึงราว 1 นาที ขึ้นกับ Google Drive
- ตอนนี้การ์ดที่เปิดค้างไว้ยังไม่อัปเดตเองเมื่อคนอื่นแก้ ให้ปิดแล้วเปิดใหม่
  ([#27](https://github.com/TheDevper/rodu/issues/27))
- เจอ error ให้ส่งข้อความ error เต็ม ๆ กับผลของ `rodu team` ให้เจ้าของทีม แต่ **อย่าส่ง invite**
  หรือลิงก์ที่มี `#token=`
