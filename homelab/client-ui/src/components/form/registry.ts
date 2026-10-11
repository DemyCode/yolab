import { LinkField } from "./fields";
import {
  ArrayFieldTemplate,
  FieldTemplate,
  ObjectFieldTemplate,
} from "./templates";
import {
  CheckboxWidget,
  FolderWidget,
  PasswordWidget,
  TextareaWidget,
  TunnelWidget,
  YolabTokenWidget,
} from "./widgets";

export const templates = {
  FieldTemplate,
  ObjectFieldTemplate,
  ArrayFieldTemplate,
};

export const fields = {
  LinkField,
};

export const widgets = {
  TunnelWidget,
  YolabTokenWidget,
  FolderWidget,
  PasswordWidget,
  CheckboxWidget,
  TextareaWidget,
};
