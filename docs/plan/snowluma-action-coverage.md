# SnowLuma 动作覆盖矩阵

状态：**全部排期动作已落地（2026-11-05，批次 9）**。输入为 SnowLuma 中文 API 索引的
200 个唯一动作（页面分类合计 200，英文页 219；以中文索引的实际条目为基准）。

- 已暴露：**200 个**（`ROUTES` 192 个 op / 200 个 action；5 个平台别名动作挂在
  主 op 的 `aliases` 上，不再单列 op。按群/私聊分动作的 op 会映射到两条）。
- 未暴露：**0 个**。8 个平台占位/未实现动作也按用户口径暴露成 `Unverified`
  （协议上有合法调用，只是没有可观察语义）；白名单为空，测试仍要求「非空则每条
  必须有理由」。
- 批次 9 审计后 `Unverified` 从 59 降到 **15**：批次 8 的 4 个、审计后仍无出处的
  3 个（`account.qzone.detail` / `account.qzone.unlike` / `msg.tuwen_ark`）、
  白名单清零新增的 8 个。其余 52 个按 SnowLuma 动作目录的摘要+参数改
  `Verified`。
- 风险分级逐条按动作：rkey / 文件集下载 / 闪传下载地址为 `Critical`；删除、取消、
  匿名禁言、QQ 空间代表账号对外发布为 `Destructive`；资料/在线状态/群待办为
  `Write`；纯查询为 `Read`。

## 分类汇总

| 分类 | 动作数 | 已暴露 | 未暴露（外部约束） | 未暴露（未排期） |
|---|---|---|---|---|
| 扩展 | 115 | 115 | 0 | 0 |
| 群管理 | 21 | 21 | 0 | 0 |
| 信息 | 5 | 5 | 0 | 0 |
| 群相册 | 9 | 9 | 0 | 0 |
| 流式接口 | 6 | 6 | 0 | 0 |
| 空间 | 10 | 10 | 0 | 0 |
| 群文件 | 12 | 12 | 0 | 0 |
| 好友 | 3 | 3 | 0 | 0 |
| 消息 | 6 | 6 | 0 | 0 |
| 系统表情 | 4 | 4 | 0 | 0 |
| 群信息 | 7 | 7 | 0 | 0 |
| 请求 | 2 | 2 | 0 | 0 |
| **合计** | **200** | **200** | **0** | **0** |

## 逐动作

### 扩展

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `_del_group_notice` | 已暴露 | `group.notice.delete` |
| `_get_friend_dress` | 已暴露 | `friend.dress`（Unverified） |
| `_get_group_notice` | 已暴露 | `group.notice.get` |
| `_get_model_show` | 已暴露 | `account.model_show`（Unverified） |
| `_mark_all_as_read` | 已暴露 | `msg.mark_all_read`（Unverified） |
| `_send_group_notice` | 已暴露 | `group.notice.send` |
| `_set_model_show` | 已暴露 | `account.model_show_set`（Unverified） |
| `.get_word_slices` | 已暴露 | `msg.word_slices`（Unverified） |
| `add_custom_face` | 已暴露 | `face.custom_add`（两段式：先加收藏，再用 emoji_id 调 `msg.face`） |
| `bot_exit` | 已暴露 | `system.bot_exit` |
| `cancel_group_todo` | 已暴露 | `group.todo.cancel`（Unverified） |
| `check_url_safely` | 已暴露 | `system.check_url` |
| `clean_cache` | 已暴露 | `system.clean_cache` |
| `click_inline_keyboard_button` | 已暴露 | `msg.inline_button`（Unverified） |
| `complete_group_todo` | 已暴露 | `group.todo.complete`（Unverified） |
| `create_collection` | 已暴露 | `file.collection_create`（Unverified） |
| `create_flash_task` | 已暴露 | `file.flash_task_create`（Unverified） |
| `delete_custom_face` | 已暴露 | `face.custom_delete` |
| `delete_essence_msg` | 已暴露 | `group.essence.delete` |
| `delete_flash_file` | 已暴露 | `file.flash_file_delete`（Unverified） |
| `delete_group_folder` | 已暴露 | `group.file.folder_delete`（别名 `group.file.folder_delete_alias`） |
| `download_file` | 已暴露 | `file.download` |
| `download_fileset` | 已暴露 | `file.fileset_download`（Unverified，Critical） |
| `fetch_custom_face` | 已暴露 | `face.custom_list` |
| `fetch_custom_face_detail` | 已暴露 | `face.custom_detail` |
| `fetch_emoji_like` | 已暴露 | `msg.reaction_users` |
| `fetch_ptt_text` | 已暴露 | `msg.voice_text` |
| `forward_friend_single_msg` | 已暴露 | `msg.forward_single_friend`（Unverified） |
| `forward_group_single_msg` | 已暴露 | `msg.forward_single_group`（Unverified） |
| `friend_poke` | 已暴露 | `msg.poke` |
| `get_ai_characters` | 已暴露 | `group.ai_characters` |
| `get_ai_record` | 已暴露 | `group.ai_voice` |
| `get_clientkey` | 已暴露 | `account.clientkey` |
| `get_collection_list` | 已暴露 | `file.collection_list`（Unverified） |
| `get_cookies` | 已暴露 | `account.cookies` |
| `get_credentials` | 已暴露 | `account.credentials` |
| `get_csrf_token` | 已暴露 | `account.csrf_token` |
| `get_doubt_friends_add_request` | 已暴露 | `friend.doubt_list` |
| `get_emoji_likes` | 已暴露 | `msg.reaction_users`（别名 `msg.emoji_likes_alias`） |
| `get_essence_msg_list` | 已暴露 | `group.essence.list` |
| `get_file` | 已暴露 | `file.info` |
| `get_fileset_id` | 已暴露 | `file.fileset_id`（Unverified） |
| `get_fileset_info` | 已暴露 | `file.fileset_info`（Unverified） |
| `get_flash_file_list` | 已暴露 | `file.flash_file_list`（Unverified） |
| `get_flash_file_url` | 已暴露 | `file.flash_file_url`（Unverified，Critical） |
| `get_forward_msg` | 已暴露 | `msg.forward` |
| `get_friend_msg_history` | 已暴露 | `msg.history` |
| `get_friends_with_category` | 已暴露 | `friend.categories` |
| `get_group_at_all_remain` | 已暴露 | `group.at_all_remain` |
| `get_group_detail_info` | 已暴露 | `group.detail` |
| `get_group_file_system_info` | 已暴露 | `group.file.info` |
| `get_group_ignore_add_request` | 已暴露 | `group.ignored_requests`（别名 `group.ignored_requests_alias`） |
| `get_group_ignored_notifies` | 已暴露 | `group.ignored_requests` |
| `get_group_info_ex` | 已暴露 | `group.info_ex`（Unverified） |
| `get_group_msg_history` | 已暴露 | `msg.history` |
| `get_group_shut_list` | 已暴露 | `group.shut_list` |
| `get_group_signed_list` | 已暴露 | `group.signed_list` |
| `get_group_todo_list` | 已暴露 | `group.todo.list`（Unverified） |
| `get_image` | 已暴露 | `msg.image` |
| `get_mini_app_ark` | 已暴露 | `msg.mini_app_ark`（Unverified） |
| `get_msg_emoji_likes` | 已暴露 | `msg.reactions` |
| `get_online_clients` | 已暴露 | `account.online_clients`（Unverified） |
| `get_profile_like` | 已暴露 | `account.profile_likes`（Unverified） |
| `get_recent_contact` | 已暴露 | `friend.recent_contacts`（Unverified） |
| `get_record` | 已暴露 | `msg.voice` |
| `get_rkey` | 已暴露 | `account.rkey`（Unverified，Critical） |
| `get_rkey_server` | 已暴露 | `account.rkey_server`（Unverified，Critical） |
| `get_share_link` | 已暴露 | `file.share_link`（只收 fileset_id） |
| `get_unidirectional_friend_list` | 已暴露 | `friend.one_way` |
| `group_poke` | 已暴露 | `msg.poke` |
| `list_filesets` | 已暴露 | `file.fileset_list`（Unverified） |
| `mark_group_msg_as_read` | 已暴露 | `msg.mark_read_group`（别名） |
| `mark_msg_as_read` | 已暴露 | `msg.mark_read` |
| `mark_private_msg_as_read` | 已暴露 | `msg.mark_read_private`（别名） |
| `modify_custom_face` | 已暴露 | `face.custom_modify` |
| `move_custom_face_to_front` | 已暴露 | `face.custom_move_front`（Unverified） |
| `nc_get_packet_status` | 已暴露 | `system.packet_status`（Unverified） |
| `nc_get_user_status` | 已暴露 | `account.user_status`（Unverified） |
| `ocr_image` | 已暴露 | `msg.ocr` |
| `rename_flash_file` | 已暴露 | `file.flash_file_rename`（Unverified） |
| `request_decrypt_key` | 已暴露 | `account.decrypt_key` |
| `send_ark_share` | 已暴露 | `msg.ark_share`（Unverified） |
| `send_flash_msg` | 已暴露 | `file.flash_msg_send`（Unverified） |
| `send_forward_msg` | 已暴露 | `msg.send_forward` |
| `send_group_ai_record` | 已暴露 | `group.ai_voice_send` |
| `send_group_ark_share` | 已暴露 | `msg.group_ark_share`（Unverified） |
| `send_group_forward_msg` | 已暴露 | `msg.send_group_forward` |
| `send_like` | 已暴露 | `like.send` |
| `send_packet` | 已暴露 | `system.send_packet` |
| `send_poke` | 已暴露 | `msg.poke`（别名 `msg.poke_alias`） |
| `send_private_forward_msg` | 已暴露 | `msg.send_private_forward` |
| `send_tuwen_ark` | 已暴露 | `msg.tuwen_ark`（Unverified） |
| `set_diy_online_status` | 已暴露 | `account.diy_status`（Unverified） |
| `set_doubt_friends_add_request` | 已暴露 | `friend.doubt_approve` |
| `set_essence_msg` | 已暴露 | `group.essence.set` |
| `set_friend_remark` | 已暴露 | `friend.remark` |
| `set_friends_category` | 已暴露 | `friend.set_category` |
| `set_group_reaction` | 已暴露 | `group.reaction` |
| `set_group_remark` | 已暴露 | `group.remark` |
| `set_group_robot_add_option` | 已暴露 | `group.robot_add_option`（Unverified） |
| `set_group_sign` | 已暴露 | `group.sign` |
| `set_group_todo` | 已暴露 | `group.todo.set`（Unverified） |
| `set_input_status` | 已暴露 | `account.input_status`（Unverified） |
| `set_msg_emoji_like` | 已暴露 | `msg.react` |
| `set_online_status` | 已暴露 | `account.online_status`（Unverified） |
| `set_qq_avatar` | 已暴露 | `account.avatar`（Unverified） |
| `set_qq_profile` | 已暴露 | `account.profile`（Unverified） |
| `set_restart` | 已暴露 | `system.restart` |
| `set_self_longnick` | 已暴露 | `account.long_nick`（Unverified） |
| `share_group_ex` | 已暴露 | `msg.group_share_ex`（Unverified） |
| `share_peer` | 已暴露 | `msg.peer_share`（Unverified） |
| `trans_group_file` | 已暴露 | `group.file.trans`（临时群文件转同群永久存储，无目标群参数） |
| `translate_en2zh` | 已暴露 | `msg.translate`（Unverified） |
| `upload_forward_msg` | 已暴露 | `msg.forward_upload` |
| `upload_foward_msg` | 已暴露 | `msg.forward_upload_alias` |

### 群管理

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `add_group_tag` | 已暴露 | `group.tag_add`（Unverified：分类语义待实验） |
| `delete_group_tag` | 已暴露 | `group.tag_delete`（Unverified：分类语义待实验） |
| `get_group_admin_settings` | 已暴露 | `group.admin_settings` |
| `get_group_tags` | 已暴露 | `group.tag_list`（Unverified：分类语义待实验） |
| `set_group_add_option` | 已暴露 | `group.add_option`（Unverified：add_type 0/1-3 待实验） |
| `set_group_admin` | 已暴露 | `member.admin` |
| `set_group_anonymous` | 已暴露 | `group.anonymous`（Unverified） |
| `set_group_anonymous_ban` | 已暴露 | `group.anonymous_ban`（Unverified） |
| `set_group_ban` | 已暴露 | `group.mute` |
| `set_group_card` | 已暴露 | `member.card` |
| `set_group_kick` | 已暴露 | `member.kick` |
| `set_group_kick_members` | 已暴露 | `member.kick_many` |
| `set_group_leave` | 已暴露 | `group.leave` |
| `set_group_member_invite_policy` | 已暴露 | `group.invite_policy` |
| `set_group_member_permissions` | 已暴露 | `group.member_permissions` |
| `set_group_name` | 已暴露 | `group.name` |
| `set_group_new_member_history_visibility` | 已暴露 | `group.history_visibility` |
| `set_group_portrait` | 已暴露 | `group.avatar` |
| `set_group_search` | 已暴露 | `group.search`（open/close 枚举） |
| `set_group_special_title` | 已暴露 | `member.title` |
| `set_group_whole_ban` | 已暴露 | `group.mute_all` |

### 信息

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `can_send_image` | 已暴露 | `account.can_send_image`（Unverified） |
| `can_send_record` | 已暴露 | `account.can_send_record`（Unverified） |
| `get_login_info` | 已暴露 | `account.login_info` |
| `get_status` | 已暴露 | `account.status` |
| `get_version_info` | 已暴露 | `account.version` |

### 群相册

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `cancel_group_album_media_like` | 已暴露 | `group.album.unlike` |
| `del_group_album_media` | 已暴露 | `group.album.delete_media` |
| `do_group_album_comment` | 已暴露 | `group.album.comment` |
| `get_group_album_list` | 已暴露 | `group.album.list` |
| `get_group_album_media_list` | 已暴露 | `group.album.media` |
| `get_qun_album_list` | 已暴露 | `group.album.qun_list` |
| `set_group_album_media_like` | 已暴露 | `group.album.like` |
| `upload_image_to_qun_album` | 已暴露 | `group.album.upload_image` |
| `upload_video_to_qun_album` | 已暴露 | `group.album.upload_video` |

### 流式接口

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `clean_stream_temp_file` | 已暴露 | `stream.clean` |
| `download_file_image_stream` | 已暴露 | `stream.download_image` |
| `download_file_record_stream` | 已暴露 | `stream.download_record` |
| `download_file_stream` | 已暴露 | `stream.download_file` |
| `test_download_stream` | 已暴露 | `stream.test_download` |
| `upload_file_stream` | 已暴露 | `stream.upload_file` |

### 空间

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `comment_qzone` | 已暴露 | `account.qzone.comment`（Unverified） |
| `delete_qzone_msg` | 已暴露 | `account.qzone.delete`（Unverified） |
| `get_qzone_feeds` | 已暴露 | `account.qzone.feeds`（Unverified） |
| `get_qzone_msg_detail` | 已暴露 | `account.qzone.detail`（Unverified） |
| `get_qzone_msg_list` | 已暴露 | `account.qzone.list`（Unverified） |
| `like_qzone` | 已暴露 | `account.qzone.like`（Unverified） |
| `send_qzone_msg` | 已暴露 | `account.qzone.send`（Unverified） |
| `set_qzone_ban` | 已暴露 | `account.qzone.ban`（Unverified） |
| `set_qzone_msg_right` | 已暴露 | `account.qzone.right`（Unverified） |
| `unlike_qzone` | 已暴露 | `account.qzone.unlike`（Unverified） |

### 群文件

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `create_group_file_folder` | 已暴露 | `group.file.folder_create` |
| `delete_group_file` | 已暴露 | `group.file.delete` |
| `delete_group_file_folder` | 已暴露 | `group.file.folder_delete` |
| `get_group_file_url` | 已暴露 | `group.file.url` |
| `get_group_files_by_folder` | 已暴露 | `group.file.folder` |
| `get_group_root_files` | 已暴露 | `group.file.list` |
| `get_private_file_url` | 已暴露 | `file.private_url` |
| `move_group_file` | 已暴露 | `group.file.move` |
| `rename_group_file` | 已暴露 | `group.file.rename` |
| `rename_group_file_folder` | 已暴露 | `group.file.folder_rename`（new_folder_name 优先，name 回退） |
| `upload_group_file` | 已暴露 | `group.file.upload` |
| `upload_private_file` | 已暴露 | `file.private_upload`（Unverified） |

### 好友

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `delete_friend` | 已暴露 | `friend.delete` |
| `get_friend_list` | 已暴露 | `friend.list` |
| `get_stranger_info` | 已暴露 | `friend.get_info` |

### 消息

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `delete_msg` | 已暴露 | `msg.recall` |
| `get_msg` | 已暴露 | `msg.get` |
| `send_custom_face` | 已暴露 | `msg.face` |
| `send_group_msg` | 已暴露 | `msg.send` |
| `send_msg` | 已暴露 | `msg.send`（别名 `msg.send_alias`） |
| `send_private_msg` | 已暴露 | `msg.send` |

### 系统表情

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `fetch_face_entity` | 已暴露 | `face.entity` |
| `fetch_super_face_id` | 已暴露 | `face.super_id`（Unverified） |
| `fetch_sys_faces` | 已暴露 | `face.system_list` |
| `search_sys_faces` | 已暴露 | `face.system_search` |

### 群信息

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `get_group_honor_info` | 已暴露 | `group.honor` |
| `get_group_info` | 已暴露 | `group.info` |
| `get_group_list` | 已暴露 | `group.list` |
| `get_group_member_info` | 已暴露 | `member.info` |
| `get_group_member_list` | 已暴露 | `member.list` |
| `get_group_share_link` | 已暴露 | `group.share_link` |
| `get_group_system_msg` | 已暴露 | `group.system_msg` |

### 请求

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `set_friend_add_request` | 已暴露 | `friend.approve` |
| `set_group_add_request` | 已暴露 | `group.approve` |

## 清零口径

- 未排期项已清零。62 个动作全部有 op 归属；5 个平台别名动作挂在主 op 的
  `aliases` 上（action 数不减、op 数减 5），其余 57 个直接路由到 SnowLuma 动作名。
- 白名单已清零。8 个平台占位/未实现动作（`_get_model_show` / `_set_model_show` /
  `.get_word_slices` / `create_collection` / `get_recent_contact` /
  `nc_get_packet_status` / `set_group_anonymous` / `set_group_anonymous_ban`）
  协议上都有合法调用，按用户口径暴露成 `Unverified`；没有「技术上无法调用」的
  动作可留在白名单里。测试仍保留 `UNEXPOSED` 常量：非空时每条必须有理由。
- 带 `Unverified` 的 15 个 op 分两类：3 个是真没出处/目录自述待核实，8 个是平台
  mock/占位（协议可调用但无语义），4 个是批次 8 的待实验项。它们在 catalog 里
  可申请、可审批、可执行，只是直调被拒且审批私信会显示 `[UNVERIFIED]`；
  实验完成后把标记改成 `Verified`。
- 审计结论逐条写在 `adapter-snowluma.md` 的「批次 9 审计表」，包括每条查的是
  SnowLuma 动作目录的哪个条目、参数是否对齐、为什么保留 `Unverified`。
- QQ 空间写操作至少 `Destructive`：`account.qzone.send` / `comment` / `like` /
  `unlike` / `delete` / `ban` / `right` 都代表账号对外发布或改变可见性，不给 `Write`。
