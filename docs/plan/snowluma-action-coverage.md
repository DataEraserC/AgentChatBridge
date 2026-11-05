# SnowLuma 动作覆盖矩阵

状态：**对照完成（2026-11-05）**。输入为 SnowLuma 中文 API 索引的 200 个唯一动作
（页面分类合计 200，英文页 219；以中文索引的实际条目为基准）。

- 已暴露：**121 个**（`ROUTES` 118 个 op / 121 个 action，按群/私聊分动作的 op 会映射到两条）。
- 未暴露：**79 个**，其中外部约束 **16 个**、未排期 **63 个**。
- 未排期的原因分四类：下一批（QQ 空间 / 收藏闪传）、别名/等价 op 已存在、账号资料/在线状态类、其它普通能力。
- 目标：后续批次把这 63 个未排期项清零后，只剩 16 个外部约束项。

## 分类汇总

| 分类 | 动作数 | 已暴露 | 未暴露（外部约束） | 未暴露（未排期） |
|---|---|---|---|---|
| 扩展 | 115 | 59 | 8 | 48 |
| 群管理 | 21 | 14 | 7 | 0 |
| 信息 | 5 | 3 | 0 | 2 |
| 群相册 | 9 | 9 | 0 | 0 |
| 流式接口 | 6 | 6 | 0 | 0 |
| 空间 | 10 | 0 | 0 | 10 |
| 群文件 | 12 | 10 | 1 | 1 |
| 好友 | 3 | 3 | 0 | 0 |
| 消息 | 6 | 5 | 0 | 1 |
| 系统表情 | 4 | 3 | 0 | 1 |
| 群信息 | 7 | 7 | 0 | 0 |
| 请求 | 2 | 2 | 0 | 0 |
| **合计** | **200** | **121** | **16** | **63** |

## 逐动作

### 扩展

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `_del_group_notice` | 已暴露 | `group.notice.delete` |
| `_get_friend_dress` | 未暴露 | 未排期（账号资料/装扮） |
| `_get_group_notice` | 已暴露 | `group.notice.get` |
| `_get_model_show` | 未暴露 | 外部约束：平台标注为兼容 mock，无真实语义 |
| `_mark_all_as_read` | 未暴露 | 未排期（全部已读） |
| `_send_group_notice` | 已暴露 | `group.notice.send` |
| `_set_model_show` | 未暴露 | 外部约束：平台标注为占位 |
| `.get_word_slices` | 未暴露 | 外部约束：平台标注未实现 |
| `add_custom_face` | 未暴露 | 未排期（收藏表情） |
| `bot_exit` | 已暴露 | `system.bot_exit` |
| `cancel_group_todo` | 未暴露 | 未排期（群待办） |
| `check_url_safely` | 已暴露 | `system.check_url` |
| `clean_cache` | 已暴露 | `system.clean_cache` |
| `click_inline_keyboard_button` | 未暴露 | 未排期（内联键盘） |
| `complete_group_todo` | 未暴露 | 未排期（群待办） |
| `create_collection` | 未暴露 | 外部约束：平台标注未实现 |
| `create_flash_task` | 未暴露 | 未排期：收藏/闪传下一批 |
| `delete_custom_face` | 已暴露 | `face.custom_delete` |
| `delete_essence_msg` | 已暴露 | `group.essence.delete` |
| `delete_flash_file` | 未暴露 | 未排期：收藏/闪传下一批 |
| `delete_group_folder` | 未暴露 | 未排期：别名：group.file.folder_delete 已覆盖 |
| `download_file` | 已暴露 | `file.download` |
| `download_fileset` | 未暴露 | 未排期：收藏/闪传下一批 |
| `fetch_custom_face` | 已暴露 | `face.custom_list` |
| `fetch_custom_face_detail` | 已暴露 | `face.custom_detail` |
| `fetch_emoji_like` | 已暴露 | `msg.reaction_users` |
| `fetch_ptt_text` | 已暴露 | `msg.voice_text` |
| `forward_friend_single_msg` | 未暴露 | 未排期（单条转发） |
| `forward_group_single_msg` | 未暴露 | 未排期（单条转发） |
| `friend_poke` | 已暴露 | `msg.poke` |
| `get_ai_characters` | 已暴露 | `group.ai_characters` |
| `get_ai_record` | 已暴露 | `group.ai_voice` |
| `get_clientkey` | 已暴露 | `account.clientkey` |
| `get_collection_list` | 未暴露 | 未排期：收藏/闪传下一批 |
| `get_cookies` | 已暴露 | `account.cookies` |
| `get_credentials` | 已暴露 | `account.credentials` |
| `get_csrf_token` | 已暴露 | `account.csrf_token` |
| `get_doubt_friends_add_request` | 已暴露 | `friend.doubt_list` |
| `get_emoji_likes` | 未暴露 | 未排期：与 msg.reaction_users（fetch_emoji_like）重叠 |
| `get_essence_msg_list` | 已暴露 | `group.essence.list` |
| `get_file` | 已暴露 | `file.info` |
| `get_fileset_id` | 未暴露 | 未排期：收藏/闪传下一批 |
| `get_fileset_info` | 未暴露 | 未排期：收藏/闪传下一批 |
| `get_flash_file_list` | 未暴露 | 未排期：收藏/闪传下一批 |
| `get_flash_file_url` | 未暴露 | 未排期：收藏/闪传下一批 |
| `get_forward_msg` | 已暴露 | `msg.forward` |
| `get_friend_msg_history` | 已暴露 | `msg.history` |
| `get_friends_with_category` | 已暴露 | `friend.categories` |
| `get_group_at_all_remain` | 已暴露 | `group.at_all_remain` |
| `get_group_detail_info` | 已暴露 | `group.detail` |
| `get_group_file_system_info` | 已暴露 | `group.file.info` |
| `get_group_ignore_add_request` | 未暴露 | 未排期：与 group.ignored_requests 重叠 |
| `get_group_ignored_notifies` | 已暴露 | `group.ignored_requests` |
| `get_group_info_ex` | 未暴露 | 未排期（群信息扩展） |
| `get_group_msg_history` | 已暴露 | `msg.history` |
| `get_group_shut_list` | 已暴露 | `group.shut_list` |
| `get_group_signed_list` | 已暴露 | `group.signed_list` |
| `get_group_todo_list` | 未暴露 | 未排期（群待办） |
| `get_image` | 已暴露 | `msg.image` |
| `get_mini_app_ark` | 未暴露 | 未排期（Ark/卡片载荷） |
| `get_msg_emoji_likes` | 已暴露 | `msg.reactions` |
| `get_online_clients` | 未暴露 | 未排期（在线状态） |
| `get_profile_like` | 未暴露 | 未排期（账号资料） |
| `get_recent_contact` | 未暴露 | 外部约束：平台标注为占位 |
| `get_record` | 已暴露 | `msg.voice` |
| `get_rkey` | 未暴露 | 未排期（下载 rkey，后续按 critical 评估） |
| `get_rkey_server` | 未暴露 | 未排期（下载 rkey，后续按 critical 评估） |
| `get_share_link` | 未暴露 | 外部约束：只收 fileset_id，文件分享语义未成文 |
| `get_unidirectional_friend_list` | 已暴露 | `friend.one_way` |
| `group_poke` | 已暴露 | `msg.poke` |
| `list_filesets` | 未暴露 | 未排期：收藏/闪传下一批 |
| `mark_group_msg_as_read` | 未暴露 | 未排期：别名：msg.mark_read 已覆盖 |
| `mark_msg_as_read` | 已暴露 | `msg.mark_read` |
| `mark_private_msg_as_read` | 未暴露 | 未排期：别名：msg.mark_read 已覆盖 |
| `modify_custom_face` | 已暴露 | `face.custom_modify` |
| `move_custom_face_to_front` | 未暴露 | 未排期（收藏表情） |
| `nc_get_packet_status` | 未暴露 | 外部约束：平台标注为占位 |
| `nc_get_user_status` | 未暴露 | 未排期（在线状态） |
| `ocr_image` | 已暴露 | `msg.ocr` |
| `rename_flash_file` | 未暴露 | 未排期：收藏/闪传下一批 |
| `request_decrypt_key` | 已暴露 | `account.decrypt_key` |
| `send_ark_share` | 未暴露 | 未排期（Ark/卡片载荷） |
| `send_flash_msg` | 未暴露 | 未排期：收藏/闪传下一批 |
| `send_forward_msg` | 已暴露 | `msg.send_forward` |
| `send_group_ai_record` | 已暴露 | `group.ai_voice_send` |
| `send_group_ark_share` | 未暴露 | 未排期（Ark/卡片载荷） |
| `send_group_forward_msg` | 已暴露 | `msg.send_group_forward` |
| `send_like` | 已暴露 | `like.send` |
| `send_packet` | 已暴露 | `system.send_packet` |
| `send_poke` | 未暴露 | 未排期：别名：msg.poke 已覆盖（group_poke / friend_poke） |
| `send_private_forward_msg` | 已暴露 | `msg.send_private_forward` |
| `send_tuwen_ark` | 未暴露 | 未排期（Ark/卡片载荷） |
| `set_diy_online_status` | 未暴露 | 未排期（在线状态） |
| `set_doubt_friends_add_request` | 已暴露 | `friend.doubt_approve` |
| `set_essence_msg` | 已暴露 | `group.essence.set` |
| `set_friend_remark` | 已暴露 | `friend.remark` |
| `set_friends_category` | 已暴露 | `friend.set_category` |
| `set_group_reaction` | 已暴露 | `group.reaction` |
| `set_group_remark` | 已暴露 | `group.remark` |
| `set_group_robot_add_option` | 未暴露 | 未排期（群机器人加群选项） |
| `set_group_sign` | 已暴露 | `group.sign` |
| `set_group_todo` | 未暴露 | 未排期（群待办） |
| `set_input_status` | 未暴露 | 未排期（在线状态） |
| `set_msg_emoji_like` | 已暴露 | `msg.react` |
| `set_online_status` | 未暴露 | 未排期（在线状态） |
| `set_qq_avatar` | 未暴露 | 未排期（账号资料） |
| `set_qq_profile` | 未暴露 | 未排期（账号资料） |
| `set_restart` | 已暴露 | `system.restart` |
| `set_self_longnick` | 未暴露 | 未排期（账号资料） |
| `share_group_ex` | 未暴露 | 未排期（Ark/卡片载荷） |
| `share_peer` | 未暴露 | 未排期（Ark/卡片载荷） |
| `trans_group_file` | 未暴露 | 外部约束：没有目标参数，转存语义未成文 |
| `translate_en2zh` | 未暴露 | 未排期（翻译） |
| `upload_forward_msg` | 已暴露 | `msg.forward_upload` |
| `upload_foward_msg` | 已暴露 | `msg.forward_upload_alias` |

### 群管理

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `add_group_tag` | 未暴露 | 外部约束：群分类 group_class 映射未成文 |
| `delete_group_tag` | 未暴露 | 外部约束：群分类 group_class 映射未成文 |
| `get_group_admin_settings` | 已暴露 | `group.admin_settings` |
| `get_group_tags` | 未暴露 | 外部约束：群分类 group_class 映射未成文 |
| `set_group_add_option` | 未暴露 | 外部约束：add_type 取值语义未成文 |
| `set_group_admin` | 已暴露 | `member.admin` |
| `set_group_anonymous` | 未暴露 | 外部约束：平台标注未实现（返回 ok） |
| `set_group_anonymous_ban` | 未暴露 | 外部约束：平台标注未实现（返回 ok） |
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
| `set_group_search` | 未暴露 | 外部约束：两个指纹开关取值未成文 |
| `set_group_special_title` | 已暴露 | `member.title` |
| `set_group_whole_ban` | 已暴露 | `group.mute_all` |

### 信息

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `can_send_image` | 未暴露 | 未排期（账号能力探针） |
| `can_send_record` | 未暴露 | 未排期（账号能力探针） |
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
| `comment_qzone` | 未暴露 | 未排期：QQ 空间下一批 |
| `delete_qzone_msg` | 未暴露 | 未排期：QQ 空间下一批 |
| `get_qzone_feeds` | 未暴露 | 未排期：QQ 空间下一批 |
| `get_qzone_msg_detail` | 未暴露 | 未排期：QQ 空间下一批 |
| `get_qzone_msg_list` | 未暴露 | 未排期：QQ 空间下一批 |
| `like_qzone` | 未暴露 | 未排期：QQ 空间下一批 |
| `send_qzone_msg` | 未暴露 | 未排期：QQ 空间下一批 |
| `set_qzone_ban` | 未暴露 | 未排期：QQ 空间下一批 |
| `set_qzone_msg_right` | 未暴露 | 未排期：QQ 空间下一批 |
| `unlike_qzone` | 未暴露 | 未排期：QQ 空间下一批 |

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
| `rename_group_file_folder` | 未暴露 | 外部约束：文件夹与父目录名称优先级未成文 |
| `upload_group_file` | 已暴露 | `group.file.upload` |
| `upload_private_file` | 未暴露 | 未排期（私聊文件上传） |

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
| `send_msg` | 未暴露 | 未排期：别名：msg.send 已覆盖（按 message_type 路由） |
| `send_private_msg` | 已暴露 | `msg.send` |

### 系统表情

| 动作 | 状态 | op / 原因 |
|---|---|---|
| `fetch_face_entity` | 已暴露 | `face.entity` |
| `fetch_super_face_id` | 未暴露 | 未排期（系统表情格式判断） |
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

- 未排期的别名项可以由现有 op 的路由别名补齐，成本低，但仍算独立 action。
- 账号资料/在线状态、群待办、收藏表情、单条转发、Ark 卡片：只是未排期，不是文档缺失。
- 外部约束项要等 SnowLuma 补文档或实现；不能靠猜参数把 op 写进 catalog。
